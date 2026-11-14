/*
 * Copyright (c) 1999, 2012, Oracle and/or its affiliates. All rights reserved.
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

import java.io.*;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;

/** A byte buffer is a flexible array which grows when elements are
 *  appended. There are also methods to append names to byte buffers
 *  and to convert byte buffers to names.
 *
 *  <p><b>This is NOT part of any supported API.
 *  If you write code that depends on this, you do so at your own risk.
 *  This code and its internal interfaces are subject to change or
 *  deletion without notice.</b>
 */
public class ByteBuffer {

    /** vmless patch: 缓冲区可选地落在 Rust arena 上。
     *  开启后 {@link #elems} 为 null，所有字节都在原生内存里，
     *  增长只做"分配新块 + 拷贝"，旧块留待 arena 整体释放。 */
    private static RustNameArena sharedArena;
    private static boolean sharedArenaLooked;
    private static final ThreadLocal<byte[]> SCRATCH = ThreadLocal.withInitial(() -> new byte[256]);

    private static synchronized RustNameArena sharedArena() {
        if (!sharedArenaLooked) {
            sharedArenaLooked = true;
            sharedArena = RustNameArena.open();
        }
        return sharedArena;
    }

    /** An array holding the bytes in this buffer; can be grown.
     *  Null when this buffer is backed by the Rust arena.
     */
    public byte[] elems;

    /** The current number of defined bytes in this buffer.
     */
    public int length;

    /** Native storage (arena mode); null in heap mode. */
    private MemorySegment seg;
    private int capacity;

    /** Create a new byte buffer.
     */
    public ByteBuffer() {
        this(64);
    }

    /** Create a new byte buffer with an initial elements array
     *  of given size.
     */
    public ByteBuffer(int initialSize) {
        this.elems = new byte[initialSize];
        this.capacity = initialSize;
    }

    /** Create a new byte buffer using the given array for storage.
     */
    public ByteBuffer(byte[] elems) {
        this.elems = elems;
        this.capacity = elems.length;
    }

    /** vmless patch: switch this buffer to Rust-arena storage.
     *  Does nothing when the native library is unavailable. */
    public void useArena() {
        if (seg != null) return;
        RustNameArena a = sharedArena();
        if (a == null) return;
        MemorySegment p = a.alloc(Math.max(capacity, 64));
        if (p == null) return;
        if (length > 0 && elems != null) {
            MemorySegment.copy(elems, 0, p, ValueLayout.JAVA_BYTE, 0, length);
        }
        seg = p;
        capacity = (int) p.byteSize();
        elems = null;
    }

    /** vmless patch: 链式开启 arena 模式。 */
    public ByteBuffer arena() { useArena(); return this; }

    /** True when this buffer is backed by native memory. */
    public boolean isArenaBacked() {
        return seg != null;
    }

    // ---- 内部访问器 ----

    private void ensure(int needed) {
        if (needed <= capacity) return;
        int n = Math.max(capacity << 1, needed);
        if (seg != null) {
            MemorySegment np = sharedArena().alloc(n);
            if (np == null) throw new OutOfMemoryError("rust arena");
            MemorySegment.copy(seg, 0, np, 0, length);
            seg = np;
            capacity = (int) np.byteSize();
        } else {
            elems = ArrayUtils.ensureCapacity(elems, needed - 1);
            capacity = elems.length;
        }
    }

    private void put(int i, byte b) {
        if (seg != null) seg.set(ValueLayout.JAVA_BYTE, i, b);
        else elems[i] = b;
    }

    private byte get(int i) {
        return seg != null ? seg.get(ValueLayout.JAVA_BYTE, i) : elems[i];
    }

    /** vmless patch: 直接写入指定位置（ClassWriter 用），不触发增长。 */
    public void putByteAt(int index, byte b) {
        put(index, b);
    }

    private static byte[] scratch(int len) {
        byte[] s = SCRATCH.get();
        if (s.length < len) {
            s = new byte[Math.max(len, s.length << 1)];
            SCRATCH.set(s);
        }
        return s;
    }

    /** Append byte to this buffer.
     */
    public void appendByte(int b) {
        ensure(length + 1);
        put(length++, (byte)b);
    }

    /** Append `len' bytes from byte array,
     *  starting at given `start' offset.
     */
    public void appendBytes(byte[] bs, int start, int len) {
        ensure(length + len);
        if (seg != null) {
            MemorySegment.copy(bs, start, seg, ValueLayout.JAVA_BYTE, length, len);
        } else {
            System.arraycopy(bs, start, elems, length, len);
        }
        length += len;
    }

    /** Append all bytes from given byte array.
     */
    public void appendBytes(byte[] bs) {
        appendBytes(bs, 0, bs.length);
    }

    /** Append a character as a two byte number.
     */
    public void appendChar(int x) {
        ensure(length + 2);
        put(length  , (byte)((x >>  8) & 0xFF));
        put(length+1, (byte)((x      ) & 0xFF));
        length = length + 2;
    }

    /** Append an integer as a four byte number.
     */
    public void appendInt(int x) {
        ensure(length + 4);
        put(length  , (byte)((x >> 24) & 0xFF));
        put(length+1, (byte)((x >> 16) & 0xFF));
        put(length+2, (byte)((x >>  8) & 0xFF));
        put(length+3, (byte)((x      ) & 0xFF));
        length = length + 4;
    }

    /** Append a long as an eight byte number.
     */
    public void appendLong(long x) {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream(8);
        DataOutputStream bufout = new DataOutputStream(buffer);
        try {
            bufout.writeLong(x);
            appendBytes(buffer.toByteArray(), 0, 8);
        } catch (IOException e) {
            throw new AssertionError("write");
        }
    }

    /** Append a float as a four byte number.
     */
    public void appendFloat(float x) {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream(4);
        DataOutputStream bufout = new DataOutputStream(buffer);
        try {
            bufout.writeFloat(x);
            appendBytes(buffer.toByteArray(), 0, 4);
        } catch (IOException e) {
            throw new AssertionError("write");
        }
    }

    /** Append a double as a eight byte number.
     */
    public void appendDouble(double x) {
        ByteArrayOutputStream buffer = new ByteArrayOutputStream(8);
        DataOutputStream bufout = new DataOutputStream(buffer);
        try {
            bufout.writeDouble(x);
            appendBytes(buffer.toByteArray(), 0, 8);
        } catch (IOException e) {
            throw new AssertionError("write");
        }
    }

    /** Append a name encoded in Modified UTF-8.
     */
    public void appendName(Name name) {
        int utf8len = name.getUtf8Length();
        ensure(length + utf8len);
        if (seg != null) {
            byte[] tmp = scratch(utf8len);
            name.getUtf8Bytes(tmp, 0);
            appendBytes(tmp, 0, utf8len);
        } else {
            name.getUtf8Bytes(elems, length);
            length += utf8len;
        }
    }

     /** Append the content of the given input stream.
     */
    public void appendStream(InputStream input) throws IOException {
        while (true) {
            int amountToRead = Math.max(input.available(), 64);
            ensure(length + amountToRead + 1);
            int amountRead;
            if (seg != null) {
                byte[] tmp = scratch(amountToRead + 1);
                amountRead = input.read(tmp, 0, amountToRead);
                if (amountRead > 0) appendBytes(tmp, 0, amountRead);
            } else {
                amountRead = input.read(elems, length, amountToRead);
            }
            if (amountRead == -1)
                break;
            if (seg == null) length += amountRead;
            if (amountRead == amountToRead) {
                int byt = input.read();
                if (byt == -1)
                    break;
                ensure(length + 1);
                put(length++, (byte)byt);
            }
        }
    }

    /** Extract an integer at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public int getInt(int bp) throws UnderflowException {
        verifyRange(bp, 4);
        if (seg != null) {
            return seg.get(ValueLayout.JAVA_INT_UNALIGNED.withOrder(ByteOrder.BIG_ENDIAN), bp);
        }
        return
            ((elems[bp] & 0xFF) << 24) +
            ((elems[bp+1] & 0xFF) << 16) +
            ((elems[bp+2] & 0xFF) << 8) +
            (elems[bp+3] & 0xFF);
    }

    /** Extract a long integer at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public long getLong(int bp) throws UnderflowException {
        verifyRange(bp, 8);
        if (seg != null) {
            return seg.get(ValueLayout.JAVA_LONG_UNALIGNED.withOrder(ByteOrder.BIG_ENDIAN), bp);
        }
        DataInputStream elemsin =
            new DataInputStream(new ByteArrayInputStream(elems, bp, 8));
        try {
            return elemsin.readLong();
        } catch (IOException e) {
            throw new AssertionError(e);
        }
    }

    /** Extract a float at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public float getFloat(int bp) throws UnderflowException {
        verifyRange(bp, 4);
        if (seg != null) {
            int bits = seg.get(ValueLayout.JAVA_INT_UNALIGNED.withOrder(ByteOrder.BIG_ENDIAN), bp);
            return Float.intBitsToFloat(bits);
        }
        DataInputStream elemsin =
            new DataInputStream(new ByteArrayInputStream(elems, bp, 4));
        try {
            return elemsin.readFloat();
        } catch (IOException e) {
            throw new AssertionError(e);
        }
    }

    /** Extract a double at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public double getDouble(int bp) throws UnderflowException {
        verifyRange(bp, 8);
        if (seg != null) {
            long bits = seg.get(ValueLayout.JAVA_LONG_UNALIGNED.withOrder(ByteOrder.BIG_ENDIAN), bp);
            return Double.longBitsToDouble(bits);
        }
        DataInputStream elemsin =
            new DataInputStream(new ByteArrayInputStream(elems, bp, 8));
        try {
            return elemsin.readDouble();
        } catch (IOException e) {
            throw new AssertionError(e);
        }
    }

    /** Extract a character at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public char getChar(int bp) throws UnderflowException {
        verifyRange(bp, 2);
        return
            (char)(((get(bp) & 0xFF) << 8) + (get(bp+1) & 0xFF));
    }

    /** Extract a byte at position bp from elems.
     *
     * @param bp starting offset
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code bp} is negative
     */
    public byte getByte(int bp) throws UnderflowException {
        verifyRange(bp, 1);
        return get(bp);
    }

    /** Reset to zero length.
     */
    public void reset() {
        length = 0;
    }

    /** Convert contents to name.
     *  @throws InvalidUtfException if invalid Modified UTF-8 is encountered
     */
    public Name toName(Names names) throws InvalidUtfException {
        if (seg != null) {
            return names.fromUtf(toByteArray(), 0, length, Convert.Validation.STRICT);
        }
        return names.fromUtf(elems, 0, length, Convert.Validation.STRICT);
    }

    /** Verify there are at least the specified number of bytes in this buffer at the specified offset.
     *
     * @param off starting offset
     * @param len required length
     * @throws UnderflowException if there is not enough data in this buffer
     * @throws IllegalArgumentException if {@code off} or {@code len} is negative
     */
    public void verifyRange(int off, int len) throws UnderflowException {
        if (off < 0 || len < 0)
            throw new IllegalArgumentException("off=" + off + ", len=" + len);
        if (off + len < 0 || off + len > length)
            throw new UnderflowException(length);
    }

    /** vmless patch: materialize the buffer as a byte array. */
    public byte[] toByteArray() {
        if (seg == null) {
            byte[] r = new byte[length];
            System.arraycopy(elems, 0, r, 0, length);
            return r;
        }
        byte[] r = new byte[length];
        MemorySegment.copy(seg, ValueLayout.JAVA_BYTE, 0, r, 0, length);
        return r;
    }

    /** vmless patch: write this buffer's content to a stream. */
    public void writeTo(OutputStream out) throws IOException {
        if (seg != null && Boolean.getBoolean("vmless.debug")) {
            System.err.println("[vmless] arena buffer flushed: " + length + " bytes");
        }
        if (seg == null) {
            out.write(elems, 0, length);
        } else {
            byte[] chunk = new byte[Math.min(length, 8192)];
            int off = 0;
            while (off < length) {
                int n = Math.min(chunk.length, length - off);
                MemorySegment.copy(seg, ValueLayout.JAVA_BYTE, off, chunk, 0, n);
                out.write(chunk, 0, n);
                off += n;
            }
        }
    }

    /** Create a {@link java.nio.ByteBuffer} view of this instance.
     *
     *  <p>
     *  If this instance is modified, the returned buffer may no longer reflect it.
     */
    public java.nio.ByteBuffer asByteBuffer() {
        if (seg != null) {
            return seg.asByteBuffer().slice(0, length);
        }
        return java.nio.ByteBuffer.wrap(elems, 0, length);
    }

// UnderflowException

    /** Thrown when trying to read past the end of the buffer.
     */
    public static class UnderflowException extends Exception {

        private static final long serialVersionUID = 0;

        private final int length;

        public UnderflowException(int length) {
            this.length = length;
        }

        /** Get the length of the buffer, which apparently is not long enough.
         */
        public int getLength() {
            return length;
        }
    }
}
