// Matrix mixer on Tensor cores (design doc 9.8): out = matrix · in, for
// many channels at once (spatial audio, large routing matrices).
//
// matrix: outputs × inputs (f16, row-major, padded to multiples of 16)
// input:  inputs × frames  (f16, row-major: one channel per row)
// output: outputs × frames (f32)
//
// One subgroup (32 threads) computes one 16×16 tile of the output with
// cooperative matrices (VK_KHR_cooperative_matrix), stepping through the
// inputs 16 at a time; products are summed in f32.

struct Params
{
    uint inputs;   // padded to a multiple of 16
    uint outputs;  // padded to a multiple of 16
    uint frames;   // padded to a multiple of 16
    uint pad;
};

[[vk::push_constant]] ConstantBuffer<Params> params;
[[vk::binding(0)]] RWStructuredBuffer<half> matrix;
[[vk::binding(1)]] RWStructuredBuffer<half> input;
[[vk::binding(2)]] RWStructuredBuffer<float> output;

typealias A = linalg.CoopMat<half, MemoryScope.Subgroup, 16, 16, linalg.CoopMatMatrixUse.MatrixA>;
typealias B = linalg.CoopMat<half, MemoryScope.Subgroup, 16, 16, linalg.CoopMatMatrixUse.MatrixB>;
typealias C = linalg.CoopMat<float, MemoryScope.Subgroup, 16, 16, linalg.CoopMatMatrixUse.MatrixAccumulator>;
static const linalg.CoopMatMatrixLayout RowMajor = linalg.CoopMatMatrixLayout.RowMajor;

[shader("compute")]
[numthreads(32, 1, 1)]
void main(uint3 group : SV_GroupID)
{
    uint frame0 = group.x * 16;  // tile column: frames
    uint out0 = group.y * 16;    // tile row: outputs
    var acc = C(0.0);
    for (uint k = 0; k < params.inputs; k += 16)
    {
        let a = A.Load<RowMajor>(matrix, out0 * params.inputs + k, params.inputs);
        let b = B.Load<RowMajor>(input, k * params.frames + frame0, params.frames);
        acc = linalg.coopMatMulAdd<float, false>(a, b, acc);
    }
    acc.Store<RowMajor>(output, out0 * params.frames + frame0, params.frames);
}
