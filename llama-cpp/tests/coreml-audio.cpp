// On-device regression test. Link against the patched Mac libmtmd and
// libggml; set MTMD_COREML_AUDIO and MTMD_COREML_AUDIO_FRAMES=500.
// Arguments: Granite mmproj GGUF, mono 16 kHz float32 PCM, reference fixture
// from scripts/bench_bridge.cpp (frames, tokens, mel, reference embeddings).
#include "mtmd.h"
#include "ggml-backend.h"

#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <memory>
#include <stdexcept>
#include <vector>

static void require(bool ok, const char * reason) {
    if (!ok) throw std::runtime_error(reason);
}

int main(int argc, char ** argv) {
    try {
        require(argc == 4, "expected mmproj, PCM and reference fixture");
        ggml_backend_load_all();
        auto params = mtmd_context_params_default();
        params.warmup = false;
        const auto memory = mtmd_get_memory_usage(argv[1], params);
        require(!memory.backend_mem_usage.empty(), "memory estimation failed");
        size_t bytes = 0;
        for (const auto & entry : memory.backend_mem_usage) bytes += entry.second;
        require(bytes == 0, "GGUF encoder weights or compute buffers were allocated");
        std::printf("GGUF encoder/projector allocation: %zu bytes\n", bytes);
        std::unique_ptr<mtmd_context, decltype(&mtmd_free)> ctx(
            mtmd_init_from_file(argv[1], nullptr, params), mtmd_free);
        require(bool(ctx), "metadata-only context initialization failed");
        require(mtmd_support_audio(ctx.get()), "missing audio support");
        require(mtmd_get_audio_sample_rate(ctx.get()) == 16000, "wrong sample rate");

        std::ifstream input(argv[2], std::ios::binary | std::ios::ate);
        require(bool(input), "cannot open PCM");
        const auto pcm_bytes = input.tellg();
        require(pcm_bytes > 0 && pcm_bytes % sizeof(float) == 0, "invalid PCM size");
        std::vector<float> pcm(size_t(pcm_bytes) / sizeof(float));
        input.seekg(0);
        input.read(reinterpret_cast<char *>(pcm.data()), pcm_bytes);
        require(bool(input), "cannot read PCM");

        std::ifstream fixture(argv[3], std::ios::binary);
        uint32_t frames = 0, tokens = 0;
        fixture.read(reinterpret_cast<char *>(&frames), sizeof frames);
        fixture.read(reinterpret_cast<char *>(&tokens), sizeof tokens);
        require(bool(fixture) && frames > 0 && frames <= 500 && tokens > 0, "bad fixture");
        fixture.seekg(size_t(160) * frames * sizeof(float), std::ios::cur);
        std::vector<float> reference(size_t(tokens) * 2048);
        fixture.read(reinterpret_cast<char *>(reference.data()), reference.size() * sizeof(float));
        require(bool(fixture), "cannot read reference embeddings");

        auto chunks_for = [&](const std::vector<float> & audio) {
            std::unique_ptr<mtmd_bitmap, decltype(&mtmd_bitmap_free)> bitmap(
                mtmd_bitmap_init_from_audio(audio.size(), audio.data()), mtmd_bitmap_free);
            std::unique_ptr<mtmd_input_chunks, decltype(&mtmd_input_chunks_free)> chunks(
                mtmd_input_chunks_init(), mtmd_input_chunks_free);
            const mtmd_input_part part{nullptr, bitmap.get()};
            const mtmd_input_part * parts[] = {&part};
            require(mtmd_tokenize_from_parts(ctx.get(), chunks.get(), parts, 1, false) == 0,
                    "audio preprocessing failed");
            require(mtmd_input_chunks_size(chunks.get()) == 1, "expected one audio chunk");
            return chunks;
        };
        auto chunks = chunks_for(pcm);
        const auto * chunk = mtmd_input_chunks_get(chunks.get(), 0);
        require(mtmd_input_chunk_get_type(chunk) == MTMD_INPUT_CHUNK_TYPE_AUDIO, "wrong chunk type");
        require(mtmd_input_chunk_get_n_tokens(chunk) == tokens, "wrong audio token count");
        double cosine = 0;
        for (int i = 0; i < 3; ++i) {
            const auto start = std::chrono::steady_clock::now();
            require(mtmd_encode_chunk(ctx.get(), chunk) == 0, "Core ML audio encoding failed");
            const auto elapsed = std::chrono::duration<double, std::milli>(
                std::chrono::steady_clock::now() - start).count();
            const float * output = mtmd_get_output_embd(ctx.get());
            require(output != nullptr, "missing embeddings");
            double aa = 0, bb = 0, ab = 0;
            for (size_t j = 0; j < reference.size(); ++j) {
                require(std::isfinite(output[j]), "non-finite embeddings");
                aa += double(reference[j]) * reference[j];
                bb += double(output[j]) * output[j];
                ab += double(reference[j]) * output[j];
            }
            cosine = ab / std::sqrt(aa * bb);
            require(cosine > 0.98, "embeddings differ from the reference");
            std::printf("prediction %d: %.1f ms, cosine %.6f\n", i, elapsed, cosine);
        }
        // Unsupported shapes must return an error; no fallback weights exist.
        pcm.resize(11 * 16000);
        auto too_long = chunks_for(pcm);
        require(mtmd_encode_chunk(ctx.get(), mtmd_input_chunks_get(too_long.get(), 0)) != 0,
                "an unsupported shape did not fail explicitly");
        std::puts("PASS: metadata dimensions, zero GGUF allocation, real audio embeddings, explicit shape failure");
        return 0;
    } catch (const std::exception & e) {
        std::fprintf(stderr, "FAIL: %s\n", e.what());
        return 1;
    }
}
