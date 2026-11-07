// A gain plugin in C#: everything Ferritron needs is in this file.
using System.Runtime.CompilerServices;
using Ferritron;

public sealed class Gain : Plugin
{
    static readonly PluginInfo Info = new("dev.ferritron.examples.csharp-gain", "Ferritron C# Gain", "Ztraqto");

    static readonly Param[] Params =
    {
        new("gain", "Gain", "dB", -60, 12, 0),
        new("invert", "Invert", "", 0, 1, 0, Steps: 1),
    };

    public override void Process(in Block block)
    {
        float gain = Dsp.DbToGain(block.Param(0));
        if (block.Param(1) >= 0.5)
            gain = -gain;
        for (int c = 0; c < block.Channels; c++)
            foreach (ref float sample in block.Channel(c))
                sample *= gain;
    }

    [ModuleInitializer]
    internal static void Register() => Host.Register<Gain>(Info, Params);
}
