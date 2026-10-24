# llama.cpp at a pinned release, with patches for Granite Speech 5.0
# (patches/, each meant to go upstream):
#   0001  the audio encoder casts its depthwise conv weights to F32, so
#         an mmproj quantized with plain llama-quantize (which stores them
#         as F16) no longer aborts in ssm_conv, on Metal or the CPU
#   0002  llama-quantize keeps those weights in F32 in the first place
#
# Not nixpkgs' llama-cpp: Granite Speech needs b11852 or later (older
# releases lack its chat template).
#
# Outputs: out has llama-server, llama-mtmd-cli (one file, no server),
# llama-quantize and the shared libraries (libllama, libmtmd, libggml*);
# dev has their headers and pkg-config files (llama.pc, mtmd.pc).
#
# On Linux the CPU backend is built for each x86 generation and picked at run
# time (lib/libggml-cpu-*.so). For aarch64-darwin it is cross-compiled
# on Linux with Metal (its shaders embedded and compiled at run time) and
# Accelerate. The caller supplies the cross toolchain:
#   native.callPackage ./llama-cpp { stdenv = crossStdenv; sdk = appleSdk; }
# where crossStdenv's cc is an aarch64-apple-darwin clang and sdk the macOS
# SDK it uses (see lib.macLlamaCpp in flake.nix).
{
  lib,
  stdenv,
  fetchFromGitHub,
  cmake,
  ninja,
  llvmPackages,
  # The macOS SDK, only for a cross build to Darwin.
  sdk ? null,
}:

let
  build = "11852";
  host = stdenv.hostPlatform;
  cross = host != stdenv.buildPlatform;
  crossDarwin = cross && host.isDarwin;
  tool = name: "${stdenv.cc}/bin/${stdenv.cc.targetPrefix}${name}";
  # Linux: next to the libraries; on the Mac there is only one CPU backend.
  backendDir = "${placeholder "out"}/lib";
in
assert crossDarwin -> sdk != null;
stdenv.mkDerivation {
  pname = "llama-cpp-granite";
  version = "b${build}";

  src = fetchFromGitHub {
    owner = "ggml-org";
    repo = "llama.cpp";
    # tag b11852
    rev = "7894e8b92481a8bb8ffe94f73b656ffe0dd01e10";
    hash = "sha256-fFso/m/CHo08Y3Dy24urdczKQmfAVb5NqLVzJUhafPM=";
  };

  patches = [
    ./patches/0001-granite-speech-conv-dw-f32.patch
    ./patches/0002-quantize-keep-conv-dw-f32.patch
  ];

  outputs = [
    "out"
    "dev"
  ];

  nativeBuildInputs = [
    cmake
    ninja
  ];

  cmakeFlags = [
    (lib.cmakeBool "GGML_NATIVE" false)
    (lib.cmakeBool "BUILD_SHARED_LIBS" true)
    (lib.cmakeBool "LLAMA_BUILD_TESTS" false)
    (lib.cmakeBool "LLAMA_BUILD_EXAMPLES" false)
    (lib.cmakeBool "LLAMA_BUILD_APP" false)
    (lib.cmakeBool "LLAMA_BUILD_TOOLS" true)
    (lib.cmakeBool "LLAMA_BUILD_SERVER" true)
    # No web UI and no downloads: only local programs talk to the server.
    (lib.cmakeBool "LLAMA_BUILD_UI" false)
    (lib.cmakeBool "LLAMA_USE_PREBUILT_UI" false)
    (lib.cmakeBool "LLAMA_OPENSSL" false)
    (lib.cmakeBool "LLAMA_FATAL_WARNINGS" false)
    (lib.cmakeFeature "LLAMA_BUILD_NUMBER" build)
  ]
  ++ lib.optionals host.isx86_64 [
    (lib.cmakeBool "GGML_CPU_ALL_VARIANTS" true)
    (lib.cmakeBool "GGML_BACKEND_DL" true)
    (lib.cmakeFeature "GGML_BACKEND_DIR" backendDir)
  ]
  ++ lib.optionals host.isDarwin [
    (lib.cmakeBool "GGML_METAL" true)
    (lib.cmakeBool "GGML_METAL_EMBED_LIBRARY" true)
    (lib.cmakeBool "GGML_BLAS" true)
    (lib.cmakeFeature "GGML_BLAS_VENDOR" "Apple")
    # Apple's clang has no OpenMP; ggml's own thread pool is used instead.
    (lib.cmakeBool "GGML_OPENMP" false)
    # M2 / A15 and later (dot product, int8 matrix multiply, FP16
    # arithmetic). Only matters for what runs on the CPU; the model runs
    # on Metal.
    (lib.cmakeFeature "GGML_CPU_ARM_ARCH" "armv8.6-a+fp16")
    (lib.cmakeFeature "CMAKE_OSX_DEPLOYMENT_TARGET" "15.0")
  ]
  ++ lib.optionals crossDarwin [
    # The cross compiler is a plain clang, not a cc-wrapper, so CMake is
    # told everything the setup hooks would otherwise export.
    (lib.cmakeFeature "CMAKE_C_COMPILER" (tool "cc"))
    (lib.cmakeFeature "CMAKE_CXX_COMPILER" (tool "c++"))
    (lib.cmakeFeature "CMAKE_ASM_COMPILER" (tool "cc"))
    (lib.cmakeFeature "CMAKE_AR" (tool "ar"))
    (lib.cmakeFeature "CMAKE_RANLIB" (tool "ranlib"))
    (lib.cmakeFeature "CMAKE_INSTALL_NAME_TOOL" "${llvmPackages.llvm}/bin/llvm-install-name-tool")
    (lib.cmakeFeature "CMAKE_OSX_SYSROOT" "${sdk}")
    # Link with the final paths; nothing needs changing at install, which
    # would invalidate the linker's ad-hoc signatures.
    (lib.cmakeBool "CMAKE_BUILD_WITH_INSTALL_RPATH" true)
    (lib.cmakeFeature "CMAKE_INSTALL_RPATH" "${placeholder "out"}/lib")
    (lib.cmakeFeature "CMAKE_INSTALL_NAME_DIR" "${placeholder "out"}/lib")
  ]
  # The Metal backend asks for macOS 15's features with @available, which
  # below a deployment target of 15 needs compiler-rt, and the cross
  # toolchain links none; at 15 the checks are compiled away. The
  # toolchain's clang passes --target=arm64-apple-macosx14.0, which beats
  # -mmacosx-version-min, so a later --target sets 15.
  ++ lib.optionals crossDarwin (
    map (lang: lib.cmakeFeature "CMAKE_${lang}_FLAGS" "--target=arm64-apple-macosx15.0") [
      "C"
      "CXX"
      "OBJC"
      "ASM"
    ]
  );

  # Linux's strip would damage Mach-O files.
  dontStrip = crossDarwin;

  postInstall = ''
    # The tools used with Granite Speech; the rest share their libraries.
    find "$out/bin" -type f -name 'llama-*' \
      ! -name llama-server ! -name llama-mtmd-cli ! -name llama-quantize \
      -delete
    find "$out/bin" -type f ! -name 'llama-*' -delete
    # Each tool is a small binary around a library of its own.
    find "$out/lib" -name 'libllama-*-impl.*' \
      ! -name 'libllama-server-impl.*' ! -name 'libllama-quantize-impl.*' \
      -delete

    # llama.pc covers libllama; libmtmd has none upstream.
    cat > "$out/lib/pkgconfig/mtmd.pc" <<EOF
    prefix=$out
    libdir=$out/lib
    includedir=$dev/include

    Name: mtmd
    Description: llama.cpp multimodal (audio, image) input
    Version: ${build}
    Requires: llama
    Libs: -L\''${libdir} -lmtmd
    Cflags: -I\''${includedir}
    EOF
  '';

  passthru = { inherit build; };

  meta = {
    description = "llama.cpp with fixes for IBM Granite Speech";
    homepage = "https://github.com/ggml-org/llama.cpp";
    license = lib.licenses.mit;
    mainProgram = "llama-server";
    platforms = lib.platforms.linux ++ [ "aarch64-darwin" ];
  };
}
