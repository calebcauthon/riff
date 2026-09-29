 - Server transcription median: 1.01s
  - p95: 4.19s
  - Worst outlier: 109s
  - Redundant server-health overhead median: 53ms
  - Recorder shutdown median: 104ms

  So the normal audio-to-transcript floor is roughly 1.15s before hooks and reporting. Full stop median is 2.94s because output hooks add another median 1.60s, although that is outside the requested ASR scope.

  ## Highest-priority findings

  1. The server can silently use the wrong model

  Health checks only search the response for "ok": true; they do not validate model, device, runtime, or ownership in src/transcription.rs:214. Worse, the client sends a model in the transcription request, but the Python server ignores it and uses whatever was
  loaded at startup in scripts/parakeet_transcribe.py:654.

  Rust then records the requested model as if it were the actual model in src/transcription.rs:632. This can invalidate benchmarks and make --parakeet-model ineffective while an existing server is healthy.

  Fix this before any new bakeoff:

  - Parse /health as JSON and require the exact model/device.
  - Return actual model, revision, device, PID, and runtime version from /transcribe.
  - Reject mismatches rather than silently proceeding.
  - Prefer a Riff-owned socket or per-runtime port.

  2. Native Apple Silicon inference is the biggest opportunity

  Parakeet v2 is a 600M-parameter model optimized primarily for NVIDIA GPUs. NVIDIA reports strong accuracy—6.05 average WER—but its published throughput is GPU-based, not representative of this Mac. NVIDIA model card
  (https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2)

  The same 18.6s recording took about 714ms in my warm CPU microbenchmark, or roughly 26× real time. FluidAudio’s native Core ML implementation reports:

  - Parakeet 110M: 96.5× real time on M2
  - Parakeet v3: around 207× real time on M4 Pro

  Those are project-reported figures and need reproduction on this M4 Air, but they indicate a plausible 4–8× inference improvement. FluidAudio model documentation (https://github.com/FluidInference/FluidAudio/blob/main/Documentation/Models.md), benchmarks
  (https://github.com/FluidInference/FluidAudio/blob/main/Documentation/Benchmarks.md)

  The lowest-risk experiment is a Swift/Core ML helper behind the existing RIFF_TRANSCRIBE_CMD interface. That avoids changing Riff’s control plane until latency and transcript quality are proven.

  3. Benchmark the 110M hybrid model before changing defaults

  nvidia/parakeet-tdt_ctc-110m is about 114M parameters, includes punctuation/capitalization, and reports 7.49 average WER versus v2’s 6.05. It is a promising speed/accuracy middle ground, especially using CTC or Core ML. NVIDIA 110M model card
  (https://huggingface.co/nvidia/parakeet-tdt_ctc-110m)

  The repository already lists it in scripts/run_nemo_model_bakeoff.sh:20, but no results exist and the model is not currently cached. I would test three profiles on the same real dictation corpus:

  - 110M Core ML — likely speed default
  - 600M v2 Core ML — likely accuracy default
  - Current 600M NeMo CPU — baseline

  The newer multilingual v3 model is not automatically a better English latency choice; it remains 600M and primarily adds 25-language support. NVIDIA v3 model card (https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3)

  4. The optional live-transcription path redoes work at stop

  The Python watcher tracks progress in its private next_start_sec, but never updates Rust’s transcription_cursor_sec. After waiting for the watcher, stop calls process_manual_chunk using the stale Rust cursor—usually zero—in src/session_commands.rs:1568. That
  can transcribe the entire recording again and append it to the watcher-generated transcript.

  This is both a latency and transcript-duplication bug. Before using live transcription to hide stop latency:

  - Derive the flush cursor from the maximum successful transcript_chunk.end_sec.
  - Transcribe only the unprocessed tail.
  - Ideally stream PCM through the existing persistent server rather than loading a second NeMo model in the watcher.

  The watcher also repeatedly re-runs ffprobe, volumedetect, and silencedetect over growing audio regions in scripts/parakeet_transcribe.py:327. An incremental VAD/streaming encoder would scale better.

  5. The warm failure path can make a bad situation much worse

  Stop may wait 30 seconds for the server, then launch a one-shot process while the original server is still loading in src/transcription.rs:328. That can produce two simultaneous 600M model loads on a 16GB machine.

  The POST request itself has no timeout in src/transcription.rs:569, which likely contributes to extreme tail latency.

  Recommended behavior:

  - Add a bounded request deadline based on audio duration.
  - Never load two copies concurrently.
  - If the server process is alive and loading, keep waiting or terminate it before fallback.
  - Record queue time separately from actual inference time.

  ## Smaller immediate wins

  - Call model.transcribe(..., use_lhotse=False, verbose=False). On the test clip this saved about 13ms steady-state and 144ms on the first request. Current invocation is at scripts/parakeet_transcribe.py:213.
  - batch_size=4 has little meaning when each request contains exactly one audio file. The microbenchmark found batch 1 and 4 effectively equivalent.
  - Keep PyTorch at four CPU threads. Six threads was about 22% slower and ten about 63% slower on this M4.
  - Replace the three hot-path curl health checks with one identity-aware check—or optimistically POST and only perform recovery on connection failure. That can recover roughly 50ms.
  - Reduce recorder exit polling from 100ms to 10–20ms or use process notification/waiting. Current polling adds an almost fixed ~100ms in src/main.rs:1484.
  - Pin a checkpoint commit or local .nemo artifact. revision: main in src/setup.rs:18 is mutable and makes cold-start behavior and accuracy less reproducible.
  - Align the setup runtime with the runtime actually used. The repository pins NeMo 2.4/PyTorch 2.7.1, while current sessions use an external NeMo 2.7.2/PyTorch 2.11 environment.

  ## Recommended sequence

  1. Fix server identity validation, request timeouts, and the live-tail duplication bug.
  2. Add actual model/device/revision plus audio duration and RTFx to performance logs.
  3. Prototype FluidAudio/Core ML through RIFF_TRANSCRIBE_CMD.
  4. Bake off 110M and 600M Core ML against the current NeMo CPU path using real dictation audio and WER review.
  5. Make native Core ML the Apple Silicon default if quality holds; retain NeMo as a compatibility fallback.

  Apple’s fully on-device SpeechAnalyzer is another compelling future backend, with system-managed and automatically updated models plus native streaming, but it requires macOS 26; this machine currently runs macOS 15.6. Apple SpeechAnalyzer overview
  (https://developer.apple.com/videos/play/wwdc2025/277/)