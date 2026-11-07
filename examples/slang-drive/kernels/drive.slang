// Soft saturation, written once for the CPU and the GPU.
// args.values[0]: drive (linear gain), args.values[1]: output gain.
float process(float x, KernelArgs args)
{
    float drive = max(args.values[0], 1e-6);
    return tanh(x * drive) / tanh(drive) * args.values[1];
}
