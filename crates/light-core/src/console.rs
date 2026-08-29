//! Turning console bytes into lines. The parsing of lines into commands is the application's.

use heapless::String;

/// Accumulates bytes into lines. Handles CR, LF and CRLF terminators, backspace, and lines
/// longer than `N`, which are discarded whole and counted rather than delivered truncated --
/// half a command is worse than no command.
pub struct LineReader<const N: usize> {
        line: String<N>,
        overflowed: bool,
        pub dropped_lines: u32,
}

impl<const N: usize> Default for LineReader<N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<const N: usize> LineReader<N> {
        pub const fn new() -> Self {
                Self { line: String::new(), overflowed: false, dropped_lines: 0 }
        }

        /// Feed one byte; a completed non-empty line comes back when its terminator arrives.
        pub fn push(&mut self, byte: u8) -> Option<String<N>> {
                match byte {
                        b'\r' | b'\n' => {
                                let overflowed = core::mem::take(&mut self.overflowed);
                                let line = core::mem::take(&mut self.line);
                                if overflowed {
                                        self.dropped_lines += 1;
                                        None
                                } else if line.is_empty() {
                                        //   CRLF: the LF arrives with nothing accumulated. a
                                        // blank line is not a command either way
                                        None
                                } else {
                                        Some(line)
                                }
                        }
                        0x08 | 0x7f => {
                                self.line.pop();
                                None
                        }
                        0x20..=0x7e => {
                                if self.line.push(byte as char).is_err() {
                                        self.overflowed = true;
                                }
                                None
                        }
                        //   other control bytes and anything non-ASCII: ignored
                        _ => None,
                }
        }

        pub fn feed<'a>(&'a mut self, bytes: &'a [u8]) -> impl Iterator<Item = String<N>> + 'a {
                bytes.iter().filter_map(move |&b| self.push(b))
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        fn lines<const N: usize>(input: &[u8]) -> (Vec<std::string::String>, u32) {
                let mut r = LineReader::<N>::new();
                let out = r.feed(input).map(|l| l.as_str().into()).collect();
                (out, r.dropped_lines)
        }

        #[test]
        fn terminators_and_blank_lines() {
                let (got, _) = lines::<16>(b"one\r\ntwo\nthree\r\r\n\nfour");
                assert_eq!(got, ["one", "two", "three"]);
        }

        #[test]
        fn backspace_edits_the_line() {
                let (got, _) = lines::<16>(b"helo\x08lo\n");
                assert_eq!(got, ["hello"]);
        }

        #[test]
        fn an_over_long_line_is_dropped_whole_and_counted() {
                let (got, dropped) = lines::<4>(b"toolong\nok\n");
                assert_eq!(got, ["ok"]);
                assert_eq!(dropped, 1);
        }
}
