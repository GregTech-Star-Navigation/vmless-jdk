/*
 * Copyright (c) 1999, 2025, Oracle and/or its affiliates. All rights reserved.
 * DO NOT ALTER OR REMOVE COPYRIGHT NOTICES OR THIS FILE HEADER.
 *
 * This code is free software; you can redistribute it and/or modify it
 * under the terms of the GNU General Public License version 2 only, as
 * published by the Free Software Foundation.  Oracle designates this
 * particular file as subject to the "Classpath" exception as provided
 * by Oracle in the LICENSE file that accompanied this code.
 *
 * This code is distributed in the hope that it will be useful, but WITHOUT
 * ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
 * FITNESS FOR A PARTICULAR PURPOSE.  See the GNU General Public License
 * version 2 for more details (a copy is included in the LICENSE file that
 * accompanied this code).
 *
 * You should have received a copy of the GNU General Public License version
 * 2 along with this work; if not, write to the Free Software Foundation,
 * Inc., 51 Franklin St, Fifth Floor, Boston, MA 02110-1301 USA.
 *
 * Please contact Oracle, 500 Oracle Parkway, Redwood Shores, CA 94065 USA
 * or visit www.oracle.com if you need additional information or have any
 * questions.
 */

package com.sun.tools.javac.util;

import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.ref.SoftReference;

/**
 * Implementation of Name.Table that stores all names in a single shared
 * byte array, expanding it as needed. This avoids the overhead incurred
 * by using an array of bytes for each name.
 *
 * vmless patch: the shared byte pool is allocated from a Rust-owned arena
 * (see {@link RustNameArena}) when available, so the name bytes live outside
 * the Java heap and are released deterministically. If the native arena is
 * unavailable the original heap pool is used and behaviour is unchanged.
 *
 * <p><b>This is NOT part of any supported API.
 * If you write code that depends on this, you do so at your own risk.
 * This code and its internal interfaces are subject to change or
 * deletion without notice.</b>
 */
public class SharedNameTable extends Utf8NameTable {
    // maintain a freelist of recently used name tables for reuse.
    private static List<SoftReference<SharedNameTable>> freelist = List.nil();

    public static synchronized SharedNameTable create(Names names) {
        while (freelist.nonEmpty()) {
            SharedNameTable t = freelist.head.get();
            freelist = freelist.tail;
            if (t != null) {
                return t;
            }
        }
        return new SharedNameTable(names);
    }

    private static synchronized void dispose(SharedNameTable t) {
        freelist = freelist.prepend(new SoftReference<>(t));
    }

    /** The hash table for names.
     */
    private NameImpl[] hashes;

    /** The shared byte array holding all encountered names. Unused (null) when
     *  the Rust arena is active; kept for the heap fallback path.
     */
    public byte[] bytes;

    /** Rust-owned pool, or null when falling back to {@link #bytes}. */
    private RustNameArena arena;
    private MemorySegment pool;
    private long poolCapacity;

    /** Scratch buffer used to convert char[] input before copying it into the
     *  off-heap pool. Bounded by the longest single name.
     */
    private byte[] scratch = new byte[256];

    /** Number of compilation units (name tables) currently using the arena. */
    private static int arenaUsers;

    /** The mask to be used for hashing
     */
    private int hashMask;

    /** The number of filled bytes in `names'.
     */
    private int nc = 0;

    /** Allocator
     *  @param names The main name table
     *  @param hashSize the (constant) size to be used for the hash table
     *                  needs to be a power of two.
     *  @param nameSize the initial size of the name table.
     *  @throws IllegalArgumentException if {@code hashSize} is not a power of two
     */
    public SharedNameTable(Names names, int hashSize, int nameSize) {
        super(names);
        if (Integer.bitCount(hashSize) != 1)
            throw new IllegalArgumentException();   // hashSize is not a power of two
        hashMask = hashSize - 1;
        hashes = new NameImpl[hashSize];
        bytes = new byte[nameSize];
        initArena(nameSize);
    }

    public SharedNameTable(Names names) {
        this(names, 0x8000, 0x20000);
    }

    private void initArena(int initialSize) {
        arena = RustNameArena.open();
        if (arena != null) {
            MemorySegment p = arena.alloc(initialSize);
            if (p != null) {
                pool = p;
                poolCapacity = initialSize;
                bytes = null;           // off-heap pool in use
                arenaUsers++;
                if (Boolean.getBoolean("vmless.debug")) {
                    System.err.println("[vmless] name pool -> Rust arena, initial=" + initialSize);
                }
            } else {
                arena.close();
                arena = null;
            }
        }
    }

    /** Grow the pool so that at least {@code needed} bytes fit. */
    private void ensureCapacity(int needed) {
        if (pool == null) {
            bytes = ArrayUtils.ensureCapacity(bytes, needed);
            return;
        }
        if (needed <= poolCapacity) {
            return;
        }
        long newCapacity = Math.max(poolCapacity * 2, needed);
        MemorySegment grown = arena.alloc(newCapacity);
        if (grown == null) {
            // out of native memory: degrade to the heap pool
            byte[] heap = new byte[(int) newCapacity];
            MemorySegment.copy(pool, 0, MemorySegment.ofArray(heap), 0, nc);
            bytes = heap;
            pool = null;
            return;
        }
        MemorySegment.copy(pool, 0, grown, 0, nc);
        pool = grown;
        poolCapacity = newCapacity;
    }

    private byte byteAt(int index) {
        return pool != null ? pool.get(ValueLayout.JAVA_BYTE, index) : bytes[index];
    }

    /** Copy one name's bytes out of the pool into a fresh array. */
    private byte[] copyName(int index, int length) {
        byte[] result = new byte[length];
        if (pool != null) {
            MemorySegment.copy(pool, index, MemorySegment.ofArray(result), 0, length);
        } else {
            System.arraycopy(bytes, index, result, 0, length);
        }
        return result;
    }

    private int hashValueAt(int index, int len) {
        int h = 0;
        for (int i = 0; i < len; i++) {
            h = h * 31 + (byteAt(index + i) & 0xff);
        }
        return h;
    }

    private boolean equalsAt(int index, byte[] other, int off, int len) {
        for (int i = 0; i < len; i++) {
            if (byteAt(index + i) != other[off + i]) {
                return false;
            }
        }
        return true;
    }

// Name.Table

    @Override
    public Name fromChars(char[] cs, int start, int len) {
        int needed = len * 3;
        if (scratch.length < needed) {
            scratch = new byte[needed];
        }
        int nbytes = Convert.chars2utf(cs, start, scratch, 0, len);
        try {
            return fromUtf(scratch, 0, nbytes, Convert.Validation.NONE);
        } catch (InvalidUtfException e) {
            throw new AssertionError("invalid UTF8 data", e);
        }
    }

    @Override
    public Name fromUtf(byte[] cs, int start, int len, Convert.Validation validation) throws InvalidUtfException {
        if (validation != Convert.Validation.NONE)
            Convert.utfValidate(cs, start, len, validation);
        int nc = this.nc;
        int h = hashValue(cs, start, len) & hashMask;
        NameImpl n = hashes[h];
        while (n != null &&
                (n.getByteLength() != len || !equalsAt(n.index, cs, start, len))) {
            n = n.next;
        }
        if (n == null) {
            ensureCapacity(nc + len);
            if (pool != null) {
                MemorySegment.copy(cs, start, pool, ValueLayout.JAVA_BYTE, nc, len);
            } else {
                System.arraycopy(cs, start, bytes, nc, len);
            }
            n = addName(nc, len, h);
        }
        return n;
    }

    private NameImpl addName(int index, int len, int hash) {
        NameImpl name = new NameImpl(this, index, len, hashes[hash]);
        hashes[hash] = name;
        this.nc = index + Math.max(len, 1);
        return name;
    }

    @Override
    public void dispose() {
        if (arena != null) {
            if (Boolean.getBoolean("vmless.debug")) {
                System.err.println("[vmless] arena frees " + nc + " bytes (capacity " + poolCapacity + ")");
            }
            // The pool belongs to the Rust arena: release it in one call and
            // drop every reference so a reused table starts from scratch.
            arena.close();
            arena = null;
            pool = null;
            poolCapacity = 0;
            arenaUsers--;
            nc = 0;
            hashes = new NameImpl[hashMask + 1];
            bytes = new byte[0x20000];
            initArena(0x20000);
        }
        dispose(this);
    }

    /** Number of tables currently backed by the Rust arena (diagnostics). */
    public static synchronized int arenaUsers() {
        return arenaUsers;
    }

// NameImpl

    static final class NameImpl extends Utf8NameTable.NameImpl {

        /** The next name occupying the same hash bucket.
         */
        final NameImpl next;

        /** The index where the bytes of this name are stored in the global name
         *  buffer `byte'.
         */
        final int index;

        /** The number of bytes in this name.
         */
        final int length;

        /** Lazily materialised copy of this name's bytes, used by the shared
         *  Utf8NameTable implementations (which are array based).
         */
        private byte[] materialized;

    // Constructor

        NameImpl(SharedNameTable table, int index, int length, NameImpl next) {
            super(table);
            this.index = index;
            this.length = length;
            this.next = next;
        }

    // Utf8NameTable.NameImpl

        @Override
        protected byte[] getByteData() {
            byte[] m = materialized;
            if (m == null) {
                m = ((SharedNameTable) table).copyName(index, length);
                materialized = m;
            }
            return m;
        }

        @Override
        protected int getByteOffset() {
            return 0;
        }

        @Override
        protected int getByteLength() {
            return length;
        }

        @Override
        protected int getNameIndex() {
            return index;
        }
    }
}
