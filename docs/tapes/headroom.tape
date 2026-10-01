# Can I load this model now? Exit codes for scripts.
Output docs/media/headroom.gif
Set Width 1400
Set Height 560
Source docs/tapes/_setup.tape
Source docs/tapes/_replay.tape
Type "oomtop headroom --need 13G; echo exit=$?"
Enter
Sleep 2.5s
Type "oomtop headroom --need 8G; echo exit=$?"
Enter
Sleep 2.5s
Type "oomtop headroom --need 2G && echo 'loading model…'"
Enter
Sleep 3s
