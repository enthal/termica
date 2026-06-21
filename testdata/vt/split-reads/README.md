# split-reads

The canonical regression test for "escape sequence parsed across read boundaries". Feeds the `bash-basic` byte stream one byte at a time through `TerminalState::feed(&[b])`. The resulting grid + mode flags MUST be byte-identical to the bulk-feed result.

This guards against a whole class of bugs where the parser's internal state isn't preserved across `feed()` calls — exactly the class of bug the OSC sniffer ([#28](https://github.com/enthal/termica/pull/28)) had to be carefully written to avoid. **Non-negotiable** per [spec/09-testing.md](../../../spec/09-testing.md#vt-golden-tests).

This directory's snapshot is the byte-by-byte output; the test `vt_split_reads_matches_bulk_for_bash_basic` makes the equivalence to `bash-basic` explicit as an in-process assertion, independent of file I/O.
