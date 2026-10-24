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
* Layout
* Resolution
* Refresh Rate
* Scale
* Hotplugging monitors

# Monitor number labels
While its window is active, the Displays panel asks the shell to show each monitor's number on that monitor (`ShowMonitorLabels` and `HideMonitorLabels` of `org.gnome.Shell`). In the sway session `regolith-powerd` owns `org.gnome.Shell` and passes these calls on to this daemon, which draws the labels. The daemon serves the same two methods, with the same arguments, on its own bus name `org.gnome.Mutter.DisplayConfig` at `/org/regolith/displayd/MonitorLabels`, interface `org.regolith.displayd.MonitorLabels`. This interface is internal to Regolith and may change together with `regolith-powerd`.

# What doesn't work?
* ~~Applying **any** changes whatsoever~~
* ~~Display page doesn't update on monitor change. **Workaround**: switch to some other page from the panel and back or restart `gnome-control-center`~~
* Night Light
* Screen Mirroring (sway doesn't support it and probably won't for the forseeable future).

# Contributing
Any and all contributions are welcome. Any ideas or suggestions are welcome as well.
