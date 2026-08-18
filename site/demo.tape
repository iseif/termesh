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

# --- the part nobody else has: the agent asks, you decide -------------------------
# Reached with plain keys because VHS cannot send F4: in the agent pane Enter opens the
# prompt, and an edit permission is answered with `a` or `r`.
#
# What this section is evidence for: the diff on screen is a change that has *not*
# happened yet. Rejecting it leaves the file exactly as it was — which is only true
# because the agent asked the client before writing, and is why the recorder configures
# the agent to ask (ADR-0016).
#
# Every step here waits for the screen rather than sleeping at it. The prompt refuses to
# open while a turn is running, and an earlier take typed a whole sentence into the agent
# pane instead — where the letters are keybindings, so it answered prompts nobody saw.
#
# Back to the file with the mistake first, so the proposed diff lands where it can be seen
# rather than in a tab nobody is looking at.
Ctrl+p Sleep 800ms
Type "pricing" Sleep 1000ms
Enter Sleep 1800ms

Tab Sleep 1200ms
Enter
Wait+Screen@30s /Ask the agent/
Sleep 600ms
Type "In src/pricing.rs add the doc comment /// Subtotal in cents. above fn subtotal"
Sleep 800ms
Enter

# However long the model takes. What is waited for is the prompt, not a duration.
Wait+Screen@300s /\[a\]ccept/
Sleep 3s
Screenshot site/img/agent.png
Sleep 2500ms

# Reject it. The agent never wrote, so there is nothing to undo.
Type "r"
Sleep 5s

# And again, taking it this time. The prompt will not open until the turn has finished,
# so wait for it rather than assuming.
Enter
Wait+Screen@120s /Ask the agent/
Sleep 600ms
Type "Please make that edit now"
Sleep 600ms
Enter
Wait+Screen@300s /\[a\]ccept/
Sleep 2500ms
Type "a"
Sleep 6s
