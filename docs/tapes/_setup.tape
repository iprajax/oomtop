# Shared setup: sourced by every tape. Run tapes from the repository root: `vhs docs/tapes/<name>.tape`.
Set Shell "bash"
Set FontFamily "Hack Nerd Font Mono"
Set FontSize 15
Set Theme "GruvboxDarkHard"
Set Padding 18
Set WindowBar Colorful
Set TypingSpeed 45ms
Set CursorBlink false
Hide
Type "export PATH=$PWD/target/release:$PATH PS1='\[\e[33m\]❯\[\e[0m\] ' TERM=xterm-256color COLORTERM=truecolor; unset TERM_PROGRAM; clear"
Enter
Sleep 300ms
Show
