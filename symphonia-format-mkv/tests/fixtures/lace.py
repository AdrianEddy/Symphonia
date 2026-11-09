"""Rewrite a Matroska file made by FFmpeg, whose muxer never laces, into one whose blocks are laced.

Each run of SimpleBlocks of a track in a cluster becomes Xiph-laced SimpleBlocks of up to FRAMES
frames each. Any other block (a BlockGroup, as FFmpeg writes for a block with a DiscardPadding) is
copied as it is. The EBML header, Info and Tracks are copied; SeekHead, Tags and Cues are dropped.
New Cues are written before the clusters: a cue point at each cluster without CueRelativePosition,
so that a seek by the cues reads the cluster from its start. The source must have one Segment of
known size, as FFmpeg writes to a file.

    python3 lace.py SRC DST [FRAMES=5]
"""

import struct
import sys

SEGMENT = 0x18538067
INFO = 0x1549A966
TRACKS = 0x1654AE6B
CLUSTER = 0x1F43B675
TIMESTAMP = 0xE7
SIMPLE_BLOCK = 0xA3
CRC32 = 0xBF
CUES = 0x1C53BB6B
CUE_POINT = 0xBB
CUE_TIME = 0xB3
CUE_TRACK_POSITIONS = 0xB7
CUE_TRACK = 0xF7
CUE_CLUSTER_POSITION = 0xF1

XIPH_LACING = 0b01 << 1
LACING_MASK = 0b11 << 1


def read_vint(data, pos):
    """The EBML variable-size integer at `pos` with its length marker, and its length in bytes."""
    length = 1
    while not data[pos] & (0x80 >> (length - 1)):
        length += 1
    return int.from_bytes(data[pos:pos + length], "big"), length


def children(data, start, end):
    """(ID, start, data start, end) of each element in `data[start:end]`."""
    pos = start
    while pos < end:
        eid, id_len = read_vint(data, pos)
        size, size_len = read_vint(data, pos + id_len)
        size &= (1 << (7 * size_len)) - 1
        assert size != (1 << (7 * size_len)) - 1, "an element of unknown size"
        data_start = pos + id_len + size_len
        yield eid, pos, data_start, data_start + size
        pos = data_start + size


def vint(value, length=None):
    """`value` as an EBML variable-size integer of `length` bytes, or of the fewest that hold it."""
    if length is None:
        length = 1
        while value >= (1 << (7 * length)) - 1:
            length += 1
    return ((1 << (7 * length)) | value).to_bytes(length, "big")


def element(eid, payload, size_length=None):
    return eid.to_bytes((eid.bit_length() + 7) // 8, "big") + vint(len(payload), size_length) + payload


def uint(eid, value, length=None):
    return element(eid, value.to_bytes(length or max(1, (value.bit_length() + 7) // 8), "big"))


def read_block(block):
    """(track, relative timestamp, flags, frame) of an unlaced block."""
    track, track_len = read_vint(block, 0)
    track &= (1 << (7 * track_len)) - 1
    timestamp, flags = struct.unpack_from(">hB", block, track_len)
    assert flags & LACING_MASK == 0, "a laced block"
    return track, timestamp, flags, block[track_len + 3:]


def lace(blocks):
    """A SimpleBlock of the frames of `blocks`, at the timestamp of the first, Xiph-laced if more
    than one."""
    track, timestamp, flags, _ = blocks[0]
    if len(blocks) == 1:
        return element(SIMPLE_BLOCK, vint(track) + struct.pack(">hB", timestamp, flags) + blocks[0][3])
    head = vint(track) + struct.pack(">hBB", timestamp, flags | XIPH_LACING, len(blocks) - 1)
    for _, _, _, frame in blocks[:-1]:
        head += b"\xff" * (len(frame) // 255) + bytes([len(frame) % 255])
    return element(SIMPLE_BLOCK, head + b"".join(frame for _, _, _, frame in blocks))


def relace_cluster(data, start, end, frames):
    """The cluster in `data[start:end]` with its SimpleBlocks laced: (timestamp, track of its first
    block, the cluster element)."""
    body = bytearray()
    run = []
    timestamp = track = None

    def flush():
        for k in range(0, len(run), frames):
            body.extend(lace(run[k:k + frames]))
        run.clear()

    for eid, el_start, data_start, el_end in children(data, start, end):
        if eid == SIMPLE_BLOCK:
            block = read_block(data[data_start:el_end])
            if run and run[0][0] != block[0]:
                flush()
            run.append(block)
            track = block[0] if track is None else track
            continue
        flush()
        if eid == TIMESTAMP:
            timestamp = int.from_bytes(data[data_start:el_end], "big")
        # The cluster's CRC-32 no longer holds.
        if eid != CRC32:
            body.extend(data[el_start:el_end])
    flush()
    assert timestamp is not None and track is not None, "a cluster without timestamp or blocks"
    return timestamp, track, element(CLUSTER, bytes(body))


def main():
    src, dst = sys.argv[1], sys.argv[2]
    frames = int(sys.argv[3]) if len(sys.argv) > 3 else 5
    data = open(src, "rb").read()

    ebml, segment = list(children(data, 0, len(data)))
    assert segment[0] == SEGMENT, "not one EBML header and one Segment"

    head = b""
    clusters = []
    for eid, start, data_start, end in children(data, segment[2], segment[3]):
        if eid in (INFO, TRACKS):
            head += data[start:end]
        elif eid == CLUSTER:
            clusters.append(relace_cluster(data, data_start, end, frames))

    def cues(positions):
        points = b"".join(
            element(CUE_POINT, uint(CUE_TIME, timestamp) + element(
                CUE_TRACK_POSITIONS, uint(CUE_TRACK, track) + uint(CUE_CLUSTER_POSITION, pos, 8)))
            for (timestamp, track, _), pos in zip(clusters, positions))
        return element(CUES, points)

    # The cluster positions are written in 8 bytes, so the size of the Cues does not depend on them.
    positions = []
    pos = len(head) + len(cues([0] * len(clusters)))
    for _, _, cluster in clusters:
        positions.append(pos)
        pos += len(cluster)

    body = head + cues(positions) + b"".join(cluster for _, _, cluster in clusters)
    with open(dst, "wb") as out:
        out.write(data[ebml[1]:ebml[3]] + element(SEGMENT, body, 8))


if __name__ == "__main__":
    main()
