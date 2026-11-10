// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io::{Cursor, ErrorKind};

use crate::io::{FiniteStream, MediaSourceStream, ReadBytes, SeekBuffered};

use super::ScopedStream;

/// A stream of 1000 bytes, each the low byte of its position, read up to 200: a scope made on it
/// begins at 200, and the whole stream is buffered, before the scope as after it.
fn stream_at_200() -> MediaSourceStream<'static> {
    let data: Vec<u8> = (0..1000).map(|pos| pos as u8).collect();
    let mut mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    mss.ignore_bytes(200).unwrap();
    mss
}

fn is_out_of_bounds<T>(result: std::io::Result<T>) -> bool {
    result.is_err_and(|err| err.kind() == ErrorKind::UnexpectedEof)
}

/// A buffered seek counts the bytes read from where it lands, within the scope: a seek back gives
/// back the bytes it unreads, a seek forward counts those it passes, and the scope ends where it
/// did.
#[test]
fn verify_scoped_stream_buffered_seek() {
    let mut mss = stream_at_200();
    let mut scoped = ScopedStream::new(&mut mss, 100);

    scoped.read_buf_exact(&mut [0; 60]).unwrap();

    assert_eq!(scoped.seek_buffered(200), 200);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (0, 100));

    assert_eq!(scoped.seek_buffered(230), 230);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (30, 70));

    // Clamped to the scope.
    assert_eq!(scoped.seek_buffered(0), 200);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (0, 100));

    assert_eq!(scoped.seek_buffered(1000), 300);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (100, 0));
    assert!(is_out_of_bounds(scoped.read_byte()));

    // The whole scope reads again from its start, and no further.
    scoped.seek_buffered(200);

    let mut all = [0; 100];
    scoped.read_buf_exact(&mut all).unwrap();
    assert!(all.iter().copied().eq((200..300).map(|pos| pos as u8)));
    assert!(is_out_of_bounds(scoped.read_byte()));
}

/// A relative buffered seek counts the bytes read from where it lands, and lands no further back
/// than the start of the scope and no further on than its end, also after a seek back.
#[test]
fn verify_scoped_stream_relative_buffered_seek() {
    let mut mss = stream_at_200();
    let mut scoped = ScopedStream::new(&mut mss, 100);

    scoped.read_buf_exact(&mut [0; 60]).unwrap();

    assert_eq!(scoped.seek_buffered_rel(-50), 210);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (10, 90));

    // The bytes before the scope are buffered too.
    assert_eq!(scoped.seek_buffered_rel(-50), 200);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (0, 100));

    assert_eq!(scoped.seek_buffered_rel(500), 300);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (100, 0));
    assert!(is_out_of_bounds(scoped.read_byte()));

    scoped.seek_buffered(200);
    assert_eq!(scoped.seek_buffered_rel(-10), 200);

    scoped.seek_buffered_rev(10);
    assert_eq!(scoped.pos(), 200);
    assert_eq!(scoped.read_byte().unwrap(), 200);
}

/// Ignoring the rest of the scope reads to its end: nothing is left to read.
#[test]
fn verify_scoped_stream_ignore() {
    let mut mss = stream_at_200();
    let mut scoped = ScopedStream::new(&mut mss, 100);

    scoped.ignore_bytes(30).unwrap();
    scoped.ignore().unwrap();

    assert_eq!(scoped.pos(), 300);
    assert_eq!((scoped.bytes_read(), scoped.bytes_available()), (100, 0));
    assert!(is_out_of_bounds(scoped.read_byte()));
}
