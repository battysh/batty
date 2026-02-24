# Batty — Hierarchical Agent Command System
# VHS tape file for recording a terminal GIF demo
# Run with: vhs demo.tape

Output demo.gif

# Terminal settings
Set FontSize 14
Set Width 1200
Set Height 600
Set Theme "Catppuccin Mocha"
Set Padding 20
Set TypingSpeed 40ms

# --- Scene 1: Show help ---
Sleep 500ms
Type "batty --help"
Sleep 300ms
Enter
Sleep 4s

# --- Scene 2: List all phase boards ---
Type "batty list"
Sleep 300ms
Enter
Sleep 4s

# --- Scene 3: Show resolved configuration ---
Type "batty config"
Sleep 300ms
Enter
Sleep 4s

# --- Scene 4: Dry-run a phase to show launch context ---
Type "batty work docs-update --dry-run"
Sleep 300ms
Enter
Sleep 5s

# Pause so viewers can read the final output
Sleep 2s
