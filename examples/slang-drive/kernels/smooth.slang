// A one-pole smoother: a stateful kernel. It keeps its last output per
// channel, across samples and blocks. args.values[0]: coefficient (0..1).
float process(float x, KernelArgs args, inout KernelState state)
{
    state.v[0] += (x - state.v[0]) * args.values[0];
    return state.v[0];
}
