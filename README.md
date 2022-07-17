# Build

```
cargo build --release 
```

# Installation
```
cargo install --path .
```

# Running the daemon
Simply run the daemon with the following command:

```
regolith-displayd
```

# Usage
Run the daemon with the command specified above. You can then use `gnome-control-center` or variants of it to manage display settings.

# What works?
* Layout dropdown
* Resolution dropdown
* Refresh rate dropdown
* Scaling dopdown
* Get display name.

# What doesn't work?
* Applying **any** changes whatsoever.
* Display page doesn't update on monitor change. **Workaround**: switch to some other page from the panel and back or restart `gnome-control-center`.
* Night Light
* Screen Mirroring (sway doesn't support it and probably won't for the forseeable future).

# Contributing
Any and all contributions are welcome. Any ideas or suggestions are welcome as well.