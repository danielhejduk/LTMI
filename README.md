# LTMI
Lock The Monk In
## Description
Minimal non-standalone linux wayland compositor that helps you with your productivity. One fullscreen app and a timer.  
The timer only runs while the LTMI window is focused, so time spent in other windows doesn't count.
## Installation
Compilation:  
`cargo build --release`  
## Usage
No configuration files, only flags.  
`./ltmi -p chromium brave # Spawns a brave window`
### Flags
`-t`, `--time` - Set the timer: `30m`, `2h`  
`-p`, `--preset` - Argument preset: `chromium`, `blank`  
`-o`, `--offline` - Cut the app off from the network  
### Why did I make this?
I have a hard time concentrating on my work so I wanna make a hard way for me to procrastinate.
