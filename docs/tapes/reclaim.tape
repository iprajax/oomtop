# What is idle and costing memory? Dry-run reclaim, then why.
Output docs/media/reclaim.gif
Set Width 1400
Set Height 560
Source docs/tapes/_setup.tape
Source docs/tapes/_replay.tape
Type "oomtop reclaim --dry-run"
Enter
Sleep 3s
Type "oomtop why"
Enter
Sleep 3.5s
