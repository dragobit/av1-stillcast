/* stillcast C ABI — thin shim over the Rust core.
 *
 * Contract: bytes in -> bytes out. Input is a complete IVF / OBU / Annex-B
 * encode whose leading temporal units are scanned for a keyframe-anchor TU
 * (sequence header + shown KEY_FRAME) and the golden TU coded right after
 * it — as produced by `stillcast encode` or a libaom ~1s still encode.
 * Output is a complete expanded IVF file.
 *
 * Build the shared/static lib with `cargo build --release`; artifacts are
 * target/release/libstillcast.{so,a}.
 */
#ifndef STILLCAST_H
#define STILLCAST_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Latest error message on this thread, or NULL when the last call
 * succeeded. The pointer stays valid until the next stillcast call on the
 * same thread; do not free it. */
const char *stillcast_last_error(void);

/* Expand a 2-frame IVF encode into a long static-video AV1 IVF stream.
 *
 *   input/input_len: complete input IVF bytes (non-NULL).
 *   fps:             output frame rate; 0 keeps the input timebase.
 *   total_frames:    output frame count.
 *   gop_size:        frames per GOP = keyframe distance = seek granularity;
 *                    must be >= 2.
 *   decoder_model:   declare decoder_model_info() in the sequence header.
 *   out_len:         receives the output byte count (non-NULL).
 *
 * Returns a buffer of *out_len bytes to free with stillcast_free(), or NULL
 * on error (stillcast_last_error() has the message). */
uint8_t *stillcast_expand(const uint8_t *input, size_t input_len,
                          uint32_t fps, uint64_t total_frames,
                          uint64_t gop_size, bool decoder_model,
                          size_t *out_len);

/* Free a buffer returned by stillcast_expand(). `len` must be the value the
 * call wrote into *out_len. Safe on NULL. */
void stillcast_free(uint8_t *p, size_t len);

#ifdef __cplusplus
}
#endif

#endif /* STILLCAST_H */
