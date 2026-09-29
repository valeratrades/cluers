# 20 "Stuck typing in pluely window after clicking another app" (needs GUI session)

The issue-11 planner found no code evidence for `useWindowFocus` being a fix (it was deleted), and proposed two hypotheses that need an interactive compositor session to confirm:
- H1: the transparent 600px expanded window keeps intercepting clicks/keyboard focus outside the visible UI.
- H2: a leaked screenshot capture overlay (pre-issue-05) holding focus.

Requires the owner to reproduce on Hyprland: steps, and whether the input keeps receiving keys after clicking another window's text field. Then fix accordingly.
