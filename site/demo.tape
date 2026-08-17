# VHS tape for the landing page and README. Regenerate with ./scripts/record-demo.sh
#
# Driven by real key chords against a real build, so a recording that looks right is also
# evidence the bindings work. It produces one GIF and three stills from a single pass.
#
# Two constraints shaped this, both found by recording and looking at the result:
#
#   * VHS cannot send function keys, and its `Alt+<key>` arrives at the application as a
#     plain character — an early take typed "problems" into the buffer instead of opening
#     the palette. So this uses only Ctrl chords, arrows, Enter, Escape and Tab, which are
#     delivered intact. A literal escape byte in `Type` does not work either; it is
#     stripped and the rest lands as text.
#   * `Wait` beats `Sleep` for anything asynchronous. rust-analyzer takes an unpredictable
#     few seconds on a cold target directory, and a fixed sleep either races it or pads
#     every recording with the worst case.

Output site/img/demo.gif

Set Shell bash
Set FontSize 13
Set Width 1500
Set Height 860
Set Padding 20
Set Margin 14
Set MarginFill "#17161c"
Set BorderRadius 9
Set TypingSpeed 55ms
Set Framerate 24

# Start in the demo project, with the fresh build first on PATH and a throwaway config
# root, so the recording shows neither a path from this machine nor a restored session.
Hide
Type "export PATH=$TERMESH_BIN_DIR:$PATH" Enter
Type "export XDG_CONFIG_HOME=$DEMO_CONFIG" Enter
Type "cd $DEMO_PROJECT; clear" Enter
Sleep 800ms
Show

Type "termesh ." Sleep 400ms Enter

# The shell is up once the panes have drawn.
Wait+Screen@20s /Project/

# --- let rust-analyzer reach a verdict off camera --------------------------------
# Cold analysis of even a tiny crate takes some seconds and the status bar narrates the
# phases, which is honest but not interesting. The tour below runs against a settled
# editor. It settles now: an earlier take of this recording never converged at all,
# because every file cargo wrote under `target` was reaching the language server as a
# watched-file change and restarting the analysis.
Hide
Ctrl+p Sleep 600ms
Type "pricing" Sleep 800ms
Enter
Wait+Screen@120s /1 error/
Sleep 6s
Show

Sleep 1500ms
Screenshot site/img/diagnostics.png
Sleep 2s

# --- the file tree is live, and lazy ---------------------------------------------
# Two tabs, not one: the ring is Project - Editor - Agent, and the Terminal is
# deliberately left out of it, because a focused shell owns Tab and a Terminal in the
# ring could be entered but never left by the same key. From the editor, one Tab lands on
# the agent pane — which an earlier take did, then pressed Enter into it.
Tab Sleep 700ms
Tab Sleep 900ms
Down Sleep 700ms
Enter Sleep 1400ms
Down Sleep 600ms
Down Sleep 700ms
Enter Sleep 2000ms

# --- quick open, fuzzy ------------------------------------------------------------
Ctrl+p Sleep 800ms
Type "cart" Sleep 1200ms
Enter Sleep 2000ms

# --- find within the buffer -------------------------------------------------------
Ctrl+f Sleep 700ms
Type "quantity" Sleep 1800ms
Escape Sleep 800ms

# --- git changes, with a real uncommitted edit ------------------------------------
Ctrl+g Sleep 2500ms
Screenshot site/img/git.png
Sleep 1500ms
Escape Sleep 1000ms

Sleep 2s
