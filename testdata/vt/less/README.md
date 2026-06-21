# less

`less` enters the alternate screen AND sets DECCKM (`\e[?1h`, application cursor keys) so its arrow-key handler receives SS3 sequences from the input encoder. Three lines of file content, then a `:` prompt on the bottom row (less's command prompt).

Verifies:
- `alt_screen` flag flips to `true`.
- `application_cursor` flag flips to `true` — this is what makes arrow keys behave correctly inside `less` ([#21](https://github.com/enthal/termica/pull/21)).
- Direct cursor addressing (`CSI 10;1H`) places the cursor at the bottom-left.
