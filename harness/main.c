/* C harness: creates struct A inside liba, hands it to libb, and checks
 * results. Proves the exported surface is a genuine C ABI. */
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>
#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h>
#include <IOSurface/IOSurface.h>
#endif
#ifdef __linux__
#include <sys/mman.h>
#endif

/* The SHIPPED header is the single source of truth for liba's surface —
 * including this file compiles the header against a real consumer, so any
 * signature drift fails the build instead of becoming cross-TU UB. */
#include "liba.h"

/* libb.so */
extern uint64_t b_process(AHandle *p);
extern double b_process_fast(const AHandle *p);
extern size_t b_observed_impl_size(void);
extern uint64_t b_checksum(const AHandle *p);
extern const uint8_t *b_data_ptr(const AHandle *p);
extern ABuf *b_grab_frame(const AHandle *p);
extern uint64_t b_frame_checksum(const ABuf *f);
typedef void (*BU64Cb)(void *user, uint64_t value);
extern void b_capture_checksum(const AHandle *p, BU64Cb cb, void *user);

/* Streaming subscriber state (callbacks arrive on a producer thread). */
typedef struct {
    _Atomic int count;
    _Atomic int in_order;
    ABuf *_Atomic kept;
} StreamCtx;

static void on_frame(void *user, ABuf *frame) { /* frame BORROWED */
    StreamCtx *cx = user;
    int i = atomic_fetch_add(&cx->count, 1);
    if (a_buf_map(frame).ptr[0] == (uint8_t)i)
        atomic_fetch_add(&cx->in_order, 1);
    if (i == 1) {
        a_buf_retain(frame); /* keep this one past the callback */
        atomic_store(&cx->kept, frame);
    }
}

/* A deliberately slow subscriber: sleeps longer than the stream period, so
 * under LATEST delivery it must miss frames rather than hold the producer
 * up. Counts what it actually saw. */
typedef struct {
    _Atomic int seen;
    int delay_us;
} SlowCtx;

static void on_slow(void *user, ABuf *frame) {
    SlowCtx *c = (SlowCtx *)user;
    (void)frame;
    usleep(c->delay_us);
    atomic_fetch_add(&c->seen, 1);
}

/* Cancellable completion: records whether it was invoked and with what.
 * NULL frame == cancelled. Invoked exactly once either way. */
typedef struct {
    _Atomic int calls;
    _Atomic int got_null;
    ABuf *_Atomic frame;
} CancelCtx;

static void on_cancellable(void *user, ABuf *frame) {
    CancelCtx *c = (CancelCtx *)user;
    atomic_fetch_add(&c->calls, 1);
    if (frame == NULL)
        atomic_store(&c->got_null, 1);
    else
        atomic_store(&c->frame, frame); /* OWNED */
}

static void on_capture(void *user, ABuf *frame) { /* frame OWNED */
    atomic_store((ABuf *_Atomic *)user, frame);
}

static void on_checksum(void *user, uint64_t value) {
    atomic_store((_Atomic uint64_t *)user, value + 1); /* +1: 0 = not yet */
}

static uint64_t view_checksum(ABufView v) {
    uint64_t sum = 0;
    for (size_t i = 0; i < v.len; i++)
        sum += v.ptr[i];
    return sum;
}

static int failures = 0;
#define CHECK(cond, msg)                                                       \
    do {                                                                       \
        if (cond) {                                                            \
            printf("  ok   %s\n", msg);                                        \
        } else {                                                               \
            printf("  FAIL %s\n", msg);                                        \
            failures++;                                                        \
        }                                                                      \
    } while (0)

int main(void) {
    printf("liba reports private sizeof(StructA) = %zu\n", a_impl_size());
    printf("libb observes the same via its liba binding: %zu\n\n",
           b_observed_impl_size());

    AHandle *a = a_create(40);
    CHECK(a != NULL, "a_create returns a handle");
    CHECK(a_id(a) == 40, "a_id reads back the id");
    CHECK(a_counter(a) == 0, "counter starts at 0");

    /* Opaque-handle path: libb mutates and reads A purely via liba calls. */
    uint64_t r = b_process(a);
    CHECK(a_counter(a) == 2, "libb incremented counter twice through liba");
    CHECK(r == 42, "b_process = id + counter = 42");

    /* The flagship evolution mechanism, as a consumer should use it:
     * version/size-guarded direct reads through the frozen window. */
    const ASharedV1 *sh = a_shared(a);
    CHECK(sh->abi_version >= 1 && sh->struct_size >= sizeof(ASharedV1),
          "shared window version/size guard (consumer pattern)");
    CHECK(sh->id == 40, "direct field read via the frozen window");

    /* repr(C) fast path: libb reads frozen shared fields directly. */
    double f = b_process_fast(a);
    CHECK(f == 1.5 * (40 + 2), "b_process_fast = scale * (id + counter)");

    /* --- Error handling, the panic shield, and error detail --- */
    /* Detail is thread-local and advisory; the status code is the contract.
     * Nothing has failed on this thread yet, so there is nothing to report. */
    CHECK(a_last_error_message() == NULL, "no error detail before any failure");

    /* The panic shield keeps the process alive AND, with the quiet hook
     * installed, keeps stderr clean — the message goes to the thread-local
     * instead of a backtrace dump. */
    CHECK(a_test_panic() == A_STATUS_PANIC, "internal panic caught at boundary");
    const char *pmsg = a_last_error_message();
    CHECK(pmsg != NULL && strstr(pmsg, "panic at ") == pmsg,
          "panic detail records the panic site");
    CHECK(strstr(pmsg, "deliberate internal panic") != NULL,
          "panic detail carries the payload");

    CHECK(a_try_fill(a, 0) == A_STATUS_INVALID_ARGUMENT, "invalid arg -> status code");
    CHECK(strcmp(a_last_error_message(), "invalid argument") == 0,
          "status failures record their detail too");

    /* Success must not clear the previous detail: the contract is "last
     * FAILURE", the dlerror/strerror convention, so a caller may read it
     * after an intervening successful call. */
    CHECK(a_try_fill(a, 7) == A_STATUS_OK && a_data(a).ptr[0] == 7, "valid try_fill succeeds");
    CHECK(strcmp(a_last_error_message(), "invalid argument") == 0,
          "success leaves the last-failure detail intact");

    /* --- Buffers and zero-copy --- */

    /* Borrowed-view model. */
    a_fill(a, 1);
    ABufView v = a_data(a);
    uint64_t cs1 = view_checksum(v);
    CHECK(v.len == 4096, "payload view has expected length");
    CHECK(b_checksum(a) == cs1, "libb checksums the same bytes via its view");
    CHECK(b_data_ptr(a) == v.ptr, "zero-copy: libb sees the SAME address");

    /* Refcounted-snapshot model (via libb, released via liba). */
    ABuf *frame = b_grab_frame(a);
    CHECK(a_buf_map(frame).ptr == v.ptr, "frame snapshot is zero-copy too");
    AFrameInfoV1 info = a_buf_info(frame);
    CHECK(info.rows == 32 && info.cols == 30 && info.channels == 4 &&
              info.row_stride == 128,
          "frame geometry: 32x30x4, padded 128-byte row stride");
    CHECK(info.rows * info.row_stride == a_buf_map(frame).len,
          "buffer length matches rows * row_stride");
    a_buf_retain(frame);
    a_buf_release(frame);
    CHECK(b_frame_checksum(frame) == cs1, "retain/release cycle keeps frame valid");
    CHECK(a_buf_map_mut(frame).ptr == NULL, "shared buffer refuses exclusive write");

    /* Copy-on-write: mutation after snapshot detaches the producer. */
    a_fill(a, 2);
    ABufView v2 = a_data(a);
    CHECK(a_buf_map(frame).ptr != v2.ptr, "COW: producer detached from snapshot");
    /* NB: checksums are seed-independent here (4096 is a multiple of 256),
     * so also compare actual bytes. */
    CHECK(b_frame_checksum(frame) == cs1 && a_buf_map(frame).ptr[0] == 1,
          "snapshot bytes are immutable");
    CHECK(b_checksum(a) == view_checksum(v2) && v2.ptr[0] == 2,
          "producer sees the new bytes");

    /* Caller-provided-buffer model. */
    uint8_t tmp[64];
    size_t n = a_copy_data(a, tmp, sizeof tmp);
    CHECK(n == sizeof tmp, "copy_data honours the caller's capacity");
    CHECK(tmp[0] == 2 && tmp[63] == (uint8_t)(2 + 63), "copied bytes match pattern");

    /* Snapshot lifetime is independent of its producer. */
    a_destroy(a);
    CHECK(b_frame_checksum(frame) == cs1, "frame outlives its producer");

    /* Exclusive write access once the reference is unique. */
    ABufViewMut mv = a_buf_map_mut(frame);
    CHECK(mv.ptr != NULL && mv.len == 4096, "unique buffer grants exclusive write");
    mv.ptr[0] = 0xAA;
    CHECK(a_buf_map(frame).ptr[0] == 0xAA, "written byte visible through read map");

    a_buf_release(frame);
    a = NULL;

    /* --- Callbacks and streaming --- */
    AHandle *s = a_create(50);
    StreamCtx cx = {0};
    uint64_t sid = a_subscribe(s, on_frame, &cx);
    a_stream(s, 4, 2);
    a_stream_join(s);
    CHECK(atomic_load(&cx.count) == 4, "4 frames delivered to subscriber");
    CHECK(atomic_load(&cx.in_order) == 4, "frames arrived in order (byte0 = index)");
    ABuf *kept = atomic_load(&cx.kept);
    CHECK(kept && a_buf_map(kept).ptr[0] == 1, "retained frame survives the callback");
    if (kept)
        a_buf_release(kept); /* contract: release requires a live handle */

    a_unsubscribe(s, sid);
    a_stream(s, 2, 1);
    a_stream_join(s);
    CHECK(atomic_load(&cx.count) == 4, "no deliveries after unsubscribe");

    /* Completion callback: one-shot async capture (owned frame). */
    ABuf *_Atomic captured = NULL;
    a_capture(s, on_capture, (void *)&captured);
    for (int i = 0; i < 1000 && !atomic_load(&captured); i++)
        usleep(1000);
    ABuf *cf = atomic_load(&captured);
    CHECK(cf && a_buf_map(cf).ptr[0] == 0xCA, "async capture completed with frame");
    if (cf)
        a_buf_release(cf);

    /* B's derived async C API composed over A's. */
    _Atomic uint64_t cs_plus1 = 0;
    b_capture_checksum(s, on_checksum, (void *)&cs_plus1);
    for (int i = 0; i < 1000 && !atomic_load(&cs_plus1); i++)
        usleep(1000);
    /* Capture frames are 4096 bytes of (seed + offset) % 256; 4096 is a
     * multiple of 256, so every byte value appears 16x regardless of seed:
     * sum = 16 * (0+1+...+255) = 16 * 32640 = 522240. (+1: 0 = "not yet".) */
    CHECK(atomic_load(&cs_plus1) == 522240 + 1, "b_capture_checksum completed");

    a_destroy(s);

    /* --- Capture cancellation --- */
    {
        AHandle *cp = a_create(72);

        /* Cancel while in flight: the callback still runs exactly once,
         * with NULL. Signalling through the argument rather than by not
         * calling is what lets a consumer free `user` unconditionally. */
        CancelCtx cc = {0, 0, NULL};
        uint64_t cid = a_capture2(cp, on_cancellable, &cc);
        CHECK(a_capture_cancel(cp, cid) == A_STATUS_OK, "in-flight capture cancelled");
        /* cancel BLOCKS until the callback has run — no sleep needed here,
         * which is the whole value of the blocking choice. */
        CHECK(atomic_load(&cc.calls) == 1, "cancelled capture invoked exactly once");
        CHECK(atomic_load(&cc.got_null) == 1, "cancellation signalled by a NULL frame");

        /* Cancelling an id that already completed is an ordinary race, not
         * a fault: it reports InvalidArgument and changes nothing. */
        CancelCtx cc2 = {0, 0, NULL};
        uint64_t cid2 = a_capture2(cp, on_cancellable, &cc2);
        for (int i = 0; i < 1000 && atomic_load(&cc2.calls) == 0; i++)
            usleep(1000);
        CHECK(atomic_load(&cc2.calls) == 1, "uncancelled capture completed");
        CHECK(atomic_load(&cc2.got_null) == 0, "completed capture delivered a frame");
        ABuf *cf2 = atomic_load(&cc2.frame);
        CHECK(cf2 && a_buf_map(cf2).ptr[0] == 0xCA, "completed frame is usable");
        if (cf2)
            a_buf_release(cf2);
        CHECK(a_capture_cancel(cp, cid2) == A_STATUS_INVALID_ARGUMENT,
              "cancelling a finished capture is a benign race");
        a_destroy(cp);
    }

    /* --- Backpressure: blocking vs latest delivery --- */
    {
        AHandle *bp = a_create(70);

        /* LATEST: the producer hands off to a pump thread and moves on. A
         * subscriber slower than the frame period misses frames, and the
         * count is retrievable — a struggling consumer must be
         * distinguishable from a healthy one. */
        SlowCtx slow = {0, 20000}; /* 20ms per frame vs a 1ms period */
        uint64_t sid_l = a_subscribe_with(bp, on_slow, &slow, A_DELIVERY_LATEST);
        struct timeval t0, t1;
        gettimeofday(&t0, NULL);
        a_stream(bp, 8, 1);
        a_stream_join(bp);
        gettimeofday(&t1, NULL);
        long producer_ms =
            (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_usec - t0.tv_usec) / 1000;
        /* 8 frames x 20ms = 160ms if the producer were blocked by the
         * subscriber; the stream itself is only ~8ms of sleeping. */
        CHECK(producer_ms < 100,
              "LATEST: producer is not held up by a slow subscriber");
        a_unsubscribe(bp, sid_l);
        uint64_t dropped = a_sub_dropped(bp, sid_l);
        CHECK(atomic_load(&slow.seen) + (int)dropped <= 8,
              "LATEST: seen + dropped never exceeds what was produced");
        a_destroy(bp);
    }
    {
        /* BLOCKING (the default, and what a_subscribe still does): lossless,
         * and the producer waits. Same subscriber, opposite trade. */
        AHandle *bp = a_create(71);
        SlowCtx slow = {0, 5000};
        uint64_t sid_b = a_subscribe_with(bp, on_slow, &slow, A_DELIVERY_BLOCKING);
        a_stream(bp, 4, 1);
        a_stream_join(bp);
        CHECK(atomic_load(&slow.seen) == 4, "BLOCKING: every frame delivered");
        CHECK(a_sub_dropped(bp, sid_b) == 0, "BLOCKING: nothing dropped");
        a_unsubscribe(bp, sid_b);
        a_destroy(bp);
    }

    /* --- Storage descriptors (heap / DMA-BUF / IOSurface) --- */
    AHandle *d = a_create(60);
    ABuf *hframe = a_frame(d);
    AFrameDescV1 hd = a_buf_export(hframe);
    CHECK(hd.kind == A_STORAGE_HEAP && hd.fd == -1 && hd.id == 0 && hd.len == 4096,
          "heap frames export a process-local descriptor");
    /* Descriptor v2: the by-value evolution path. A by-value struct's size
     * is baked into every compiled call site, so it can never grow — the
     * successor is a new TYPE reached by a new ENTRY POINT, and both live
     * forever. Note this TU calls BOTH against the same library. */
    AFrameDescV2 hd2 = a_buf_export2(hframe);
    CHECK(hd2.kind == hd.kind && hd2.fd == hd.fd && hd2.id == hd.id &&
              hd2.offset == hd.offset && hd2.len == hd.len,
          "v2 descriptor reproduces every v1 field");
    CHECK(hd2.fourcc == A_FOURCC_RGBA8888 && hd2.modifier == A_MODIFIER_LINEAR,
          "v2 carries the DRM format vocabulary");
    CHECK(hd2.width == 30 && hd2.height == 32 && hd2.stride == 128 && hd2.plane_count == 1,
          "v2 carries geometry, with the padded stride intact");
    /* The prefix is layout-identical (asserted in gen-header.sh), so a v2
     * descriptor can be read through a v1 pointer — which is what makes an
     * old consumer's compiled call sites keep working. */
    const AFrameDescV1 *asv1 = (const AFrameDescV1 *)&hd2;
    CHECK(asv1->kind == hd.kind && asv1->len == hd.len,
          "v2 prefix is readable as a v1 descriptor");

    a_buf_release(hframe);
    CHECK(a_set_storage(d, 99) == A_STATUS_INVALID_ARGUMENT,
          "unknown storage kind rejected cleanly");

    /* Frame-pool storage is per-producer-OUTPUT, not per-producer: a
     * capture pool feeding a GPU importer wants device buffers whether or
     * not the producer's own working payload is one. */
    CHECK(a_frame_storage(d) == A_STORAGE_HEAP, "emitted frames default to heap");
    CHECK(a_set_frame_storage(d, 99) == A_STATUS_INVALID_ARGUMENT,
          "unsupported pool kind fails eagerly, not per-frame");
#ifdef __APPLE__
    {
        CHECK(a_set_frame_storage(d, A_STORAGE_IOSURFACE) == A_STATUS_OK,
              "select IOSurface for emitted frames");
        CHECK(a_frame_storage(d) == A_STORAGE_IOSURFACE, "pool kind reported back");
        /* The producer's OWN payload is still heap — the two are orthogonal. */
        ABuf *own = a_frame(d);
        CHECK(a_buf_export(own).kind == A_STORAGE_HEAP,
              "producer payload kind is unaffected by the pool setting");
        a_buf_release(own);

        /* A captured frame comes from the pool, so it IS an IOSurface. */
        ABuf *_Atomic pooled = NULL;
        a_capture(d, on_capture, (void *)&pooled);
        for (int i = 0; i < 1000 && !atomic_load(&pooled); i++)
            usleep(1000);
        ABuf *pf = atomic_load(&pooled);
        CHECK(pf != NULL, "pooled capture completed");
        if (pf) {
            AFrameDescV2 cd = a_buf_export2(pf);
            CHECK(cd.kind == A_STORAGE_IOSURFACE && cd.id != 0,
                  "captured frame allocated from the IOSurface pool");
            CHECK(cd.fourcc == A_FOURCC_RGBA8888 && cd.stride == 128,
                  "pooled frame carries the same format/geometry contract");
            CHECK(a_buf_map(pf).ptr[0] == 0xCA, "pooled frame contents readable");
            a_buf_release(pf);
        }
        CHECK(a_set_frame_storage(d, A_STORAGE_HEAP) == A_STATUS_OK, "pool kind reset");
    }
#endif

#ifdef __APPLE__
    CHECK(a_set_storage(d, A_STORAGE_IOSURFACE) == A_STATUS_OK, "switch payload to IOSurface storage");
    a_fill(d, 5);
    ABuf *sf = a_frame(d);
    AFrameDescV1 sd = a_buf_export(sf);
    CHECK(sd.kind == A_STORAGE_IOSURFACE && sd.id != 0, "IOSurface descriptor has a global ID");
    IOSurfaceRef looked = IOSurfaceLookup(sd.id);
    CHECK(looked != NULL, "IOSurfaceLookup resolves the exported ID");
    IOSurfaceLock(looked, kIOSurfaceLockReadOnly, NULL);
    const uint8_t *base = IOSurfaceGetBaseAddress(looked);
    CHECK(base && base[0] == 5 && base[127] == (uint8_t)(5 + 127),
          "looked-up surface shows liba's bytes (zero-copy)");
    a_fill(d, 6); /* COW: the snapshot keeps its own surface */
    CHECK(base[0] == 5 && a_buf_map(sf).ptr[0] == 5,
          "snapshot surface unchanged after producer COW");
    CHECK(a_data(d).ptr[0] == 6, "producer landed on a fresh buffer");
    IOSurfaceUnlock(looked, kIOSurfaceLockReadOnly, NULL);
    CFRelease(looked);
    a_buf_release(sf);
#endif
#ifdef __linux__
    CHECK(a_set_storage(d, A_STORAGE_DMABUF) == A_STATUS_OK, "switch payload to DMA-BUF storage");
    a_fill(d, 5);
    ABuf *sf = a_frame(d);
    AFrameDescV1 sd = a_buf_export(sf);
    CHECK(sd.kind == A_STORAGE_DMABUF && sd.fd >= 0, "DMA-BUF descriptor carries a real fd");
    uint8_t *m = mmap(NULL, sd.len, PROT_READ, MAP_SHARED, sd.fd, 0);
    CHECK(m != MAP_FAILED, "exported fd mmaps independently");
    CHECK(m[0] == 5 && m[127] == (uint8_t)(5 + 127),
          "independent mapping shows liba's bytes (zero-copy)");
    a_fill(d, 6); /* COW: the snapshot keeps its own dma-buf */
    CHECK(m[0] == 5 && a_buf_map(sf).ptr[0] == 5,
          "snapshot dma-buf unchanged after producer COW");
    CHECK(a_data(d).ptr[0] == 6, "producer landed on a fresh buffer");
    munmap(m, sd.len);
    close(sd.fd);
    a_buf_release(sf);
#endif
    a_destroy(d);

    printf("\n%s (%d failures)\n", failures ? "RESULT: FAIL" : "RESULT: PASS",
           failures);
    return failures ? 1 : 0;
}
