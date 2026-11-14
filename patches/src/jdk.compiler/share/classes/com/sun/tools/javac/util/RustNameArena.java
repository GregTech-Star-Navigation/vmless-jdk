/*
 * vmless patch: Rust-backed arena for javac's shared name pool.
 *
 * The name pool of SharedNameTable is by far the largest single allocation
 * javac performs for name interning. This class routes that pool into a
 * Rust-owned arena (rustcore.dll), so the bytes live outside the Java heap
 * and are released deterministically by a single arena_free call.
 *
 * If the native library cannot be loaded (or FFM is unavailable), {@link #open}
 * returns {@code null} and the caller falls back to the original heap pool.
 */
package com.sun.tools.javac.util;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.file.Path;

/** A Rust-owned memory arena. Closing it releases every byte at once. */
@SuppressWarnings("restricted")
final class RustNameArena implements AutoCloseable {

    private static final String DEFAULT_LIB = "rustcore.dll";
    private static boolean loadTried;
    private static MethodHandle NEW;
    private static MethodHandle ALLOC;
    private static MethodHandle FREE;
    private static MethodHandle STATS;

    private final MemorySegment handle;
    private boolean closed;

    private RustNameArena(MemorySegment handle) {
        this.handle = handle;
    }

    /**
     * Open a Rust arena, or return {@code null} if the native library or the
     * foreign API is not available. Never throws.
     */
    static RustNameArena open() {
        try {
            if (!loadTried) {
                loadTried = true;
                String lib = System.getProperty("vmless.rust", DEFAULT_LIB);
                Linker linker = Linker.nativeLinker();
                SymbolLookup lookup = SymbolLookup.libraryLookup(Path.of(lib), Arena.global());
                NEW = linker.downcallHandle(lookup.find("arena_new").orElseThrow(),
                        FunctionDescriptor.of(ValueLayout.ADDRESS));
                ALLOC = linker.downcallHandle(lookup.find("arena_alloc").orElseThrow(),
                        FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));
                FREE = linker.downcallHandle(lookup.find("arena_free").orElseThrow(),
                        FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
                STATS = linker.downcallHandle(lookup.find("arena_stats").orElseThrow(),
                        FunctionDescriptor.ofVoid(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
            }
            MemorySegment h = (MemorySegment) NEW.invokeExact();
            return h.address() == 0 ? null : new RustNameArena(h);
        } catch (Throwable t) {
            if (Boolean.getBoolean("vmless.debug")) {
                System.err.println("[vmless] arena unavailable: " + t);
                t.printStackTrace();
            }
            return null;
        }
    }

    /** Allocate {@code len} bytes in the Rust arena, or return {@code null}. */
    MemorySegment alloc(long len) {
        if (closed) return null;
        try {
            MemorySegment p = (MemorySegment) ALLOC.invokeExact(handle, len);
            return p.address() == 0 ? null : p.reinterpret(len);
        } catch (Throwable t) {
            return null;
        }
    }

    /** Returns {bytes, blocks} currently held by the Rust arena. */
    long[] stats() {
        try (Arena tmp = Arena.ofConfined()) {
            MemorySegment b = tmp.allocate(ValueLayout.JAVA_LONG);
            MemorySegment k = tmp.allocate(ValueLayout.JAVA_LONG);
            STATS.invokeExact(handle, b, k);
            return new long[]{b.get(ValueLayout.JAVA_LONG, 0), k.get(ValueLayout.JAVA_LONG, 0)};
        } catch (Throwable t) {
            return new long[]{-1, -1};
        }
    }

    @Override
    public void close() {
        if (closed) return;
        closed = true;
        try {
            FREE.invokeExact(handle);
        } catch (Throwable ignore) {
            // best effort: the arena is released when the process exits
        }
    }
}
