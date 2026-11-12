// Ferritron.cs - write Ferritron plugins in C# (NativeAOT).
//
// Add this file to a NativeAOT class library (PublishAot, NativeLib=Shared,
// AllowUnsafeBlocks). It exports ferritron_plugin_entry, the C ABI of
// ferritron_abi.h; a small Rust crate with the format adapters loads the
// library as a companion (ferritron_core::companion_plugin!).
//
//     public sealed class Gain : Plugin
//     {
//         static readonly PluginInfo Info = new("com.example.gain", "Gain", "Example");
//         static readonly Param[] Params = { new("gain", "Gain", "dB", -60, 12, 0) };
//
//         public override void Process(in Block block)
//         {
//             float g = Dsp.DbToGain(block.Param(0));
//             for (int c = 0; c < block.Channels; c++)
//                 foreach (ref float s in block.Channel(c)) s *= g;
//         }
//
//         [ModuleInitializer]
//         internal static void Register() => Host.Register<Gain>(Info, Params);
//     }
//
// As in Rust and C++, parameter changes take effect at their exact sample,
// a bypass parameter is added, state is saved, and exceptions never cross
// the ABI (the plugin then outputs silence). The Rust side that loads the
// library flushes denormals around Process.
// Process runs on the audio thread: do not allocate there (the GC must not
// run during audio); Block and its spans are allocation-free.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

using System;
using System.Collections.Generic;
using System.Globalization;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;

namespace Ferritron;

public enum PluginKind : uint { Effect = 0, Instrument = 1 }

public sealed record PluginInfo(string Id, string Name, string Vendor, string Version = "0.1.0", PluginKind Kind = PluginKind.Effect,
                                string Url = "", string Email = "");

/// A parameter: plain values from Min to Max, Steps > 0 for stepped ones.
public sealed record Param(string Key, string Name, string Unit, double Min, double Max, double Default, uint Steps = 0)
{
    public double Normalize(double plain) => Max > Min ? Math.Clamp((plain - Min) / (Max - Min), 0, 1) : 0;

    public double Plain(double normalized)
    {
        double v = Min + (Max - Min) * Math.Clamp(normalized, 0, 1);
        if (Steps > 0)
        {
            double step = (Max - Min) / Steps;
            v = Min + step * Math.Round((v - Min) / step);
        }
        return v;
    }
}

/// A note in the current stretch of the block (instruments).
public readonly record struct Note(int Offset, bool On, int Key, float Velocity);

/// One stretch of the current block in which parameters are constant.
public readonly unsafe ref struct Block
{
    readonly float** channels;
    readonly int start;
    readonly double[] plain;
    readonly ReadOnlySpan<Note> notes;

    internal Block(float** channels, int count, int start, int frames, double[] plain, ReadOnlySpan<Note> notes)
    {
        this.channels = channels;
        Channels = count;
        this.start = start;
        Frames = frames;
        this.plain = plain;
        this.notes = notes;
    }

    public int Channels { get; }
    public int Frames { get; }
    public Span<float> Channel(int index) => new(channels[index] + start, Frames);
    /// Plain value of parameter `index` (in the plugin's table) in this stretch.
    public double Param(int index) => plain[index];
    /// Notes in this stretch; offsets are relative to it.
    public ReadOnlySpan<Note> Notes => notes;
}

public abstract class Plugin
{
    public virtual void Activate(double sampleRate, int maxBlock) { }
    public virtual void Reset() { }
    public abstract void Process(in Block block);
    /// Delay the plugin adds, in samples; read after Activate.
    public virtual uint Latency => 0;
}

public static class Dsp
{
    public static float DbToGain(double db) => (float)Math.Pow(10, db / 20);
}

/// Connects the plugin class to the C ABI.
public static unsafe class Host
{
    // --- ferritron_abi.h, x64 layout ---------------------------------------
    [StructLayout(LayoutKind.Sequential)] struct Str { public byte* Ptr; public uint Len; }

    [StructLayout(LayoutKind.Sequential)]
    struct Info
    {
        public Str Id, Name, Vendor, Version, Url, Email;
        public uint Kind;
        public fixed byte ClassId[16];
        public uint Isolation;
        // Added in 1.3.1 (the shims read them; leaving them out let CLAP read past the struct).
        public Str Vst3Subcategories, ClapFeatures;
        public uint AudioFlags, MinChannels, MaxChannels, SidechainChannels, Vst2Id;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct ParamInfo
    {
        public uint Id;
        public Str Name, Unit;
        public uint StepCount;
        public double DefaultNormalized;
        public uint Flags;
    }

    [StructLayout(LayoutKind.Explicit, Size = 64)]
    unsafe struct ProcessData
    {
        [FieldOffset(0)] public uint Size;
        [FieldOffset(4)] public uint Frames;
        [FieldOffset(8)] public uint NumChannels;
        [FieldOffset(12)] public uint NumEvents;
        [FieldOffset(16)] public float** Channels;
        [FieldOffset(24)] public Event* Events;
        [FieldOffset(32)] public void* Executor;
        [FieldOffset(40)] public Event* OutEvents;
        [FieldOffset(48)] public uint OutCapacity;
        [FieldOffset(56)] public uint* OutCount;
    }

    [StructLayout(LayoutKind.Explicit, Size = 24)]
    struct Event
    {
        [FieldOffset(0)] public uint Offset;
        [FieldOffset(4)] public uint Type;
        [FieldOffset(8)] public uint ParamId;
        [FieldOffset(16)] public double Normalized;
        [FieldOffset(14)] public short Key;
        [FieldOffset(16)] public float Velocity;
    }

    [StructLayout(LayoutKind.Sequential)]
    struct Api
    {
        public uint StructSize, AbiVersion;
        public Info* Info;
        public delegate* unmanaged<void*> Create;
        public delegate* unmanaged<void*, void> Destroy;
        public delegate* unmanaged<uint> ParamCount;
        public delegate* unmanaged<uint, ParamInfo*, byte> ParamInfo;
        public delegate* unmanaged<void*, uint, double> GetParam;
        public delegate* unmanaged<void*, uint, double, byte> SetParam;
        public delegate* unmanaged<uint, double, double> ToPlain;
        public delegate* unmanaged<uint, double, double> ToNormalized;
        public delegate* unmanaged<uint, double, byte*, uint, int> FormatParam;
        public delegate* unmanaged<uint, Str, double*, byte> ParseParam;
        public delegate* unmanaged<void*, double, uint, byte> Activate;
        public delegate* unmanaged<void*, ProcessData*, byte> Process;
        public delegate* unmanaged<void*, byte*, uint, uint> SaveState;
        public delegate* unmanaged<void*, byte*, uint, byte> LoadState;
        public delegate* unmanaged<byte*, void*> GetExtension;
        public delegate* unmanaged<void*, void> Reset;
    }

    [StructLayout(LayoutKind.Sequential)] struct LatencyApi { public delegate* unmanaged<void*, uint> Get; }

    const uint EventParam = 0, EventNoteOn = 1, EventNoteOff = 2;
    const uint FlagBypass = 1 << 1;
    const int MaxNotes = 512;

    // --- the registered plugin -------------------------------------------------
    static Func<Plugin>? factory;
    static PluginInfo pluginInfo = new("", "", "");
    static Param[] table = Array.Empty<Param>();
    static uint[] ids = Array.Empty<uint>();
    static Api* api;
    static LatencyApi* latencyApi;
    static readonly Dictionary<string, Str> strings = new();

    /// Registers the plugin class (call from a [ModuleInitializer]).
    public static void Register<T>(PluginInfo info, Param[] parameters) where T : Plugin, new()
    {
        factory = () => new T();
        pluginInfo = info;
        table = [.. parameters, new Param("ferritron.bypass", "Bypass", "", 0, 1, 0, 1)];
        ids = Array.ConvertAll(table, p => ParamId(p.Key));
    }

    /// Same IDs as the Rust core (FNV-1a), so plugins keep their automation
    /// whichever language they are written in.
    static ulong Fnv1a64(string text, ulong seed)
    {
        ulong hash = 0xcbf29ce484222325ul ^ seed;
        foreach (byte b in Encoding.UTF8.GetBytes(text))
        {
            hash ^= b;
            hash *= 0x00000100000001b3ul;
        }
        return hash;
    }

    static uint ParamId(string key)
    {
        ulong h = Fnv1a64(key, 0);
        return (uint)(h ^ (h >> 32)) & 0x7fffffffu;
    }

    static Str Utf8(string text)
    {
        byte[] bytes = Encoding.UTF8.GetBytes(text);
        byte* p = (byte*)NativeMemory.Alloc((nuint)Math.Max(bytes.Length, 1));
        bytes.CopyTo(new Span<byte>(p, bytes.Length));
        return new Str { Ptr = p, Len = (uint)bytes.Length };
    }

    [UnmanagedCallersOnly(EntryPoint = "ferritron_plugin_entry")]
    static void* Entry()
    {
        if (factory is null) return null;
        if (api is not null) return api;
        var info = (Info*)NativeMemory.AllocZeroed((nuint)sizeof(Info));
        info->Id = Utf8(pluginInfo.Id);
        info->Name = Utf8(pluginInfo.Name);
        info->Vendor = Utf8(pluginInfo.Vendor);
        info->Version = Utf8(pluginInfo.Version);
        info->Url = Utf8(pluginInfo.Url);
        info->Email = Utf8(pluginInfo.Email);
        info->Kind = (uint)pluginInfo.Kind;
        // Empty strings and 0 IDs: the formats' defaults. Mono or stereo, as Rust plugins without `[audio]`.
        info->MinChannels = 1;
        info->MaxChannels = 2;
        ulong a = Fnv1a64(pluginInfo.Id, 0x666572726974726ful), b = Fnv1a64(pluginInfo.Id, 0x6e00000000000001ul);
        for (int k = 0; k < 8; k++)
        {
            info->ClassId[k] = (byte)(a >> (56 - 8 * k));
            info->ClassId[k + 8] = (byte)(b >> (56 - 8 * k));
        }
        latencyApi = (LatencyApi*)NativeMemory.Alloc((nuint)sizeof(LatencyApi));
        latencyApi->Get = &GetLatency;
        var t = (Api*)NativeMemory.AllocZeroed((nuint)sizeof(Api));
        t->StructSize = (uint)sizeof(Api);
        t->AbiVersion = 2;
        t->Info = info;
        t->Create = &Create;
        t->Destroy = &Destroy;
        t->ParamCount = &ParamCount;
        t->ParamInfo = &GetParamInfo;
        t->GetParam = &GetParam;
        t->SetParam = &SetParam;
        t->ToPlain = &ToPlain;
        t->ToNormalized = &ToNormalized;
        t->FormatParam = &FormatParam;
        t->ParseParam = &ParseParam;
        t->Activate = &Activate;
        t->Process = &Process;
        t->SaveState = &SaveState;
        t->LoadState = &LoadState;
        t->GetExtension = &GetExtension;
        t->Reset = &Reset;
        api = t;
        return api;
    }

    // --- instances -------------------------------------------------------------
    sealed class Instance
    {
        public readonly Plugin Plugin;
        public readonly long[] Normalized; // double bits, read and written atomically
        public readonly double[] Plain;
        public readonly Note[] Notes = new Note[MaxNotes];
        public uint Latency;
        public bool Failed;

        public Instance(Plugin plugin)
        {
            Plugin = plugin;
            Normalized = Array.ConvertAll(table, p => BitConverter.DoubleToInt64Bits(p.Normalize(p.Default)));
            Plain = new double[table.Length - 1];
        }

        public double Get(int i) => BitConverter.Int64BitsToDouble(Volatile.Read(ref Normalized[i]));
        public void Set(int i, double v) => Volatile.Write(ref Normalized[i], BitConverter.DoubleToInt64Bits(Math.Clamp(v, 0, 1)));
        public bool Bypassed => Get(table.Length - 1) >= 0.5;
    }

    static Instance Self(void* p) => (Instance)GCHandle.FromIntPtr((nint)p).Target!;

    static int IndexOf(uint id) => Array.IndexOf(ids, id);

    [UnmanagedCallersOnly]
    static void* Create()
    {
        try { return (void*)GCHandle.ToIntPtr(GCHandle.Alloc(new Instance(factory!()))); }
        catch { return null; }
    }

    [UnmanagedCallersOnly]
    static void Destroy(void* p) => GCHandle.FromIntPtr((nint)p).Free();

    [UnmanagedCallersOnly]
    static uint ParamCount() => (uint)table.Length;

    [UnmanagedCallersOnly]
    static byte GetParamInfo(uint index, ParamInfo* info)
    {
        if (index >= table.Length || info is null) return 0;
        Param p = table[index];
        info->Id = ids[index];
        info->Name = CachedUtf8(p.Name);
        info->Unit = CachedUtf8(p.Unit);
        info->StepCount = p.Steps;
        info->DefaultNormalized = p.Normalize(p.Default);
        info->Flags = index == table.Length - 1 ? FlagBypass : 0;
        return 1;
    }

    /// Strings handed to the host stay valid for the process (one copy each).
    static Str CachedUtf8(string text)
    {
        lock (strings)
        {
            if (!strings.TryGetValue(text, out Str s))
                strings[text] = s = Utf8(text);
            return s;
        }
    }

    [UnmanagedCallersOnly]
    static double GetParam(void* p, uint id)
    {
        int i = IndexOf(id);
        return i < 0 ? 0 : Self(p).Get(i);
    }

    [UnmanagedCallersOnly]
    static byte SetParam(void* p, uint id, double normalized)
    {
        int i = IndexOf(id);
        if (i < 0) return 0;
        Self(p).Set(i, normalized);
        return 1;
    }

    [UnmanagedCallersOnly]
    static double ToPlain(uint id, double normalized)
    {
        int i = IndexOf(id);
        return i < 0 ? 0 : table[i].Plain(normalized);
    }

    [UnmanagedCallersOnly]
    static double ToNormalized(uint id, double plain)
    {
        int i = IndexOf(id);
        return i < 0 ? 0 : table[i].Normalize(plain);
    }

    [UnmanagedCallersOnly]
    static int FormatParam(uint id, double normalized, byte* buf, uint cap)
    {
        try
        {
            int i = IndexOf(id);
            if (i < 0 || buf is null || cap == 0) return -1;
            Param p = table[i];
            double v = p.Plain(normalized);
            // The value only: hosts show the unit next to it.
            string text = i == table.Length - 1 ? (v >= 0.5 ? "On" : "Off") : v.ToString(p.Steps > 0 ? "F0" : "F2", CultureInfo.InvariantCulture);
            byte[] bytes = Encoding.UTF8.GetBytes(text);
            int n = Math.Min(bytes.Length, (int)cap);
            bytes.AsSpan(0, n).CopyTo(new Span<byte>(buf, n));
            return n;
        }
        catch { return -1; }
    }

    [UnmanagedCallersOnly]
    static byte ParseParam(uint id, Str text, double* result)
    {
        try
        {
            int i = IndexOf(id);
            if (i < 0 || result is null || text.Ptr is null) return 0;
            string s = Encoding.UTF8.GetString(text.Ptr, (int)text.Len).Trim();
            if (i == table.Length - 1)
            {
                *result = s.StartsWith("On", StringComparison.OrdinalIgnoreCase) || s == "1" ? 1 : 0;
                return 1;
            }
            int end = 0;
            while (end < s.Length && (char.IsDigit(s[end]) || s[end] is '-' or '+' or '.' or 'e' or 'E')) end++;
            if (!double.TryParse(s.AsSpan(0, end), NumberStyles.Float, CultureInfo.InvariantCulture, out double v)) return 0;
            *result = table[i].Normalize(v);
            return 1;
        }
        catch { return 0; }
    }

    [UnmanagedCallersOnly]
    static void Reset(void* p)
    {
        var inst = Self(p);
        try { inst.Plugin.Reset(); }
        catch { inst.Failed = true; }
    }

    [UnmanagedCallersOnly]
    static byte Activate(void* p, double sampleRate, uint maxBlock)
    {
        var inst = Self(p);
        try
        {
            inst.Plugin.Activate(sampleRate, (int)maxBlock);
            inst.Plugin.Reset();
            inst.Latency = inst.Plugin.Latency;
            inst.Failed = false;
        }
        catch { inst.Failed = true; }
        return inst.Failed ? (byte)0 : (byte)1;
    }

    static void Run(Instance inst, float** channels, uint count, uint start, uint end, Event* events, uint numEvents)
    {
        if (end <= start) return;
        for (int i = 0; i < inst.Plain.Length; i++) inst.Plain[i] = table[i].Plain(inst.Get(i));
        int notes = 0;
        for (uint e = 0; e < numEvents && notes < MaxNotes; e++)
        {
            Event ev = events[e];
            if ((ev.Type == EventNoteOn || ev.Type == EventNoteOff) && ev.Offset >= start && ev.Offset < end)
                inst.Notes[notes++] = new Note((int)(ev.Offset - start), ev.Type == EventNoteOn, ev.Key, ev.Velocity);
        }
        var block = new Block(channels, (int)count, (int)start, (int)(end - start), inst.Plain, inst.Notes.AsSpan(0, notes));
        inst.Plugin.Process(in block);
    }

    [UnmanagedCallersOnly]
    static byte Process(void* p, ProcessData* data)
    {
        float** channels = data->Channels;
        uint count = data->NumChannels, frames = data->Frames, numEvents = data->NumEvents;
        Event* events = data->Events;
        // Denormals are flushed by the Rust side (ferritron_core::companion).
        var inst = Self(p);
        try
        {
            if (inst.Failed) throw new InvalidOperationException();
            uint at = 0;
            for (uint e = 0; e < numEvents; e++)
            {
                Event ev = events[e];
                if (ev.Type != EventParam) continue;
                uint offset = Math.Min(ev.Offset, frames);
                if (offset > at && !inst.Bypassed) Run(inst, channels, count, at, offset, events, numEvents);
                at = Math.Max(at, offset);
                int i = IndexOf(ev.ParamId);
                if (i >= 0) inst.Set(i, ev.Normalized);
            }
            if (inst.Bypassed)
            {
                if (pluginInfo.Kind == PluginKind.Instrument)
                    for (uint c = 0; c < count; c++) new Span<float>(channels[c], (int)frames).Clear();
            }
            else
            {
                Run(inst, channels, count, at, frames, events, numEvents);
            }
            return 1;
        }
        catch
        {
            inst.Failed = true;
            for (uint c = 0; c < count; c++) new Span<float>(channels[c], (int)frames).Clear();
            return 0;
        }
    }

    /// State: "FRTS", then (id, normalized) for every parameter.
    [UnmanagedCallersOnly]
    static uint SaveState(void* p, byte* buf, uint cap)
    {
        uint size = 8 + (uint)table.Length * 12;
        if (buf is not null && cap >= size)
        {
            var inst = Self(p);
            var span = new Span<byte>(buf, (int)size);
            "FRTS"u8.CopyTo(span);
            BitConverter.TryWriteBytes(span[4..], (uint)table.Length);
            for (int i = 0; i < table.Length; i++)
            {
                BitConverter.TryWriteBytes(span[(8 + i * 12)..], ids[i]);
                BitConverter.TryWriteBytes(span[(12 + i * 12)..], inst.Get(i));
            }
        }
        return size;
    }

    [UnmanagedCallersOnly]
    static byte LoadState(void* p, byte* data, uint len)
    {
        if (data is null || len < 8) return 0;
        var span = new ReadOnlySpan<byte>(data, (int)len);
        if (!span[..4].SequenceEqual("FRTS"u8)) return 0;
        uint n = BitConverter.ToUInt32(span[4..]);
        if (len < 8 + n * 12) return 0;
        var inst = Self(p);
        for (int k = 0; k < n; k++)
        {
            int i = IndexOf(BitConverter.ToUInt32(span[(8 + k * 12)..]));
            if (i >= 0) inst.Set(i, BitConverter.ToDouble(span[(12 + k * 12)..])); // removed parameters are skipped
        }
        return 1;
    }

    [UnmanagedCallersOnly]
    static uint GetLatency(void* p) => Self(p).Latency;

    [UnmanagedCallersOnly]
    static void* GetExtension(byte* id)
    {
        if (id is null) return null;
        return Marshal.PtrToStringUTF8((nint)id) == "ferritron.latency.v1" ? latencyApi : null;
    }
}
