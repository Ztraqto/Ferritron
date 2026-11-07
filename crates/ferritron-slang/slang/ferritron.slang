// What a Ferritron kernel sees. A kernel is a Slang function
//
//     float process(float x, KernelArgs args)
//
// called for every sample of every channel. It is compiled for the GPU
// (WGSL) and for the CPU (C++) from the same source (design doc 9.11).
struct KernelArgs
{
    // The kernel's parameters, set from the plugin (KernelNode::set).
    float values[8];
    uint channel;
    // Frame within the block.
    uint frame;
};

// What a stateful kernel keeps per channel, between samples and blocks
// (a filter's memory, an envelope, an oscillator's phase). A kernel is
// stateful when it is written as
//
//     float process(float x, KernelArgs args, inout KernelState state)
//
// The state is all zeros at the start and after a reset. On the GPU, one
// thread runs each channel through the block in order.
struct KernelState
{
    float v[16];
};
