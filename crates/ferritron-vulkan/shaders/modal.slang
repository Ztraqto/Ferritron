// Persistent modal bank (design doc 9.3: persistent kernels).
//
// The kernel stays on the GPU between audio blocks. Every workgroup waits
// for the host to post a block (control.seq), filters it through its lanes
// (one biquad each, state in registers), writes its partial mix to host
// memory and its done flag, then waits for the next block. It exits after a
// bounded number of blocks or idle spins (Windows resets a GPU that runs
// one dispatch for more than about 2 s), saving the lane state; the host
// keeps a second run queued behind it, so blocks never wait for a launch.

struct Control
{
    uint seq;      // last block posted by the host
    uint stop;     // nonzero: exit now
    uint frames;   // frames in the posted block
    uint reset;    // bumped by the host to clear every lane's state
};

struct Lane
{
    float b0, b1, b2, a1, a2;
    uint input;    // input channel
    uint output;   // output channel
    uint pad;
};

struct Params
{
    uint lanes;
    uint inputs;
    uint outputs;
    uint max_frames;
    uint groups;
    uint max_blocks;   // blocks per run before it hands over to the next
    uint max_spins;    // idle polls before it exits
    uint pad;
};

[[vk::push_constant]] ConstantBuffer<Params> params;

// Host writes, GPU reads (device-local, host-visible memory).
[[vk::binding(0)]] globallycoherent RWStructuredBuffer<Control> control;
[[vk::binding(1)]] globallycoherent RWStructuredBuffer<float> input;   // [inputs][max_frames]
[[vk::binding(2)]] globallycoherent RWStructuredBuffer<Lane> lanes;
// GPU writes, host reads (host memory).
[[vk::binding(3)]] globallycoherent RWStructuredBuffer<float> partial; // [groups][outputs][max_frames]
[[vk::binding(4)]] globallycoherent RWStructuredBuffer<uint> done;     // [groups]: last block finished
// GPU only: lane state and progress, kept between runs.
[[vk::binding(5)]] RWStructuredBuffer<float2> state;                   // [lanes]
[[vk::binding(6)]] globallycoherent RWStructuredBuffer<uint> progress; // [groups][2]: last block finished, reset seen

static const uint GROUP = 64;
static const uint MAX_OUTPUTS = 8;

groupshared uint g_seq;
// One partial sum per wave (waves of 8 to 64 lanes) and output.
groupshared float g_sum[GROUP / 8][MAX_OUTPUTS];

[shader("compute")]
[numthreads(GROUP, 1, 1)]
void main(uint3 group : SV_GroupID, uint3 local : SV_GroupThreadID, uint3 global : SV_DispatchThreadID)
{
    uint lane_index = global.x;
    bool active = lane_index < params.lanes;
    Lane lane = {};
    float2 z = float2(0, 0);
    if (active)
        z = state[lane_index];
    uint last = progress[group.x * 2];
    uint reset = progress[group.x * 2 + 1];
    uint wave = local.x / WaveGetLaneCount();

    for (uint block = 0; block < params.max_blocks; block++)
    {
        if (local.x == 0)
        {
            uint seq = last;
            for (uint spin = 0; spin < params.max_spins; spin++)
            {
                uint posted;
                InterlockedOr(control[0].seq, 0, posted);
                if (posted != last || control[0].stop != 0)
                {
                    seq = posted;
                    break;
                }
            }
            g_seq = control[0].stop != 0 ? last : seq;
        }
        GroupMemoryBarrierWithGroupSync();
        uint seq = g_seq;
        if (seq == last)
            break; // idle or stopping: hand over
        uint frames = min(control[0].frames, params.max_frames);
        uint wanted_reset = control[0].reset;
        if (wanted_reset != reset)
        {
            z = float2(0, 0);
            reset = wanted_reset;
        }
        if (active)
            lane = lanes[lane_index];

        for (uint f = 0; f < frames; f++)
        {
            float y = 0;
            if (active)
            {
                // Transposed direct form II.
                float x = input[lane.input * params.max_frames + f];
                y = lane.b0 * x + z.x;
                z.x = lane.b1 * x - lane.a1 * y + z.y;
                z.y = lane.b2 * x - lane.a2 * y;
            }
            for (uint o = 0; o < params.outputs; o++)
            {
                float s = WaveActiveSum(active && lane.output == o ? y : 0);
                if (WaveIsFirstLane())
                    g_sum[wave][o] = s;
            }
            GroupMemoryBarrierWithGroupSync();
            if (local.x < params.outputs)
            {
                float total = 0;
                for (uint w = 0; w < GROUP / WaveGetLaneCount(); w++)
                    total += g_sum[w][local.x];
                partial[(group.x * params.outputs + local.x) * params.max_frames + f] = total;
            }
            GroupMemoryBarrierWithGroupSync();
        }

        // Results first, then the flag that says they are there.
        DeviceMemoryBarrierWithGroupSync();
        if (local.x == 0)
            done[group.x] = seq;
        last = seq;
    }

    if (active)
        state[lane_index] = z;
    if (local.x == 0)
    {
        progress[group.x * 2] = last;
        progress[group.x * 2 + 1] = reset;
    }
}
