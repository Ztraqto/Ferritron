// A gain plugin in C++: everything Ferritron needs is in this file.
#include "ferritron.hpp"

struct Gain {
    static constexpr ferritron::Info info{"dev.ferritron.examples.cpp-gain", "Ferritron C++ Gain", "Ztraqto", "0.1.0"};

    // key, name, unit, min, max, default, steps (0 = continuous)
    static constexpr ferritron::Param params[]{
        {"gain", "Gain", "dB", -60.0, 12.0, 0.0},
        {"invert", "Invert", "", 0.0, 1.0, 0.0, 1},
    };

    void process(ferritron::Block& block) {
        float gain = ferritron::db_to_gain(block.param(0));
        if (block.param(1) >= 0.5)
            gain = -gain;
        for (auto channel : block.channels())
            for (float& sample : channel)
                sample *= gain;
    }
};

FERRITRON_PLUGIN(Gain)
