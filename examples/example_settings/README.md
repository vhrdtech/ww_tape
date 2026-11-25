# example_settings

In the ww_tape book's settings case, a tuning tool sets a motor controller's parameters, switches its profile and flashes its firmware while it records. The tape holds a keyed state track `params` (one slot per parameter path), a state track `profile` and a bytes track `firmware`.

`run` writes the session into a chain of two pieces in memory. It puts parameters by key and deletes one. It switches the profile and two gains as one group of puts, and writes a 200 000-byte image in parts. It then reads the parameters in force from the last piece alone. One parameter's history comes from the first piece, and the image comes back whole with its hash checked.

```sh
cargo run -p example_settings  # prints the report the chapters "Tracks and items" and "State" show
```

`tests/settings.rs` writes that report into `docs/generated/settings_*.txt` and checks what each read gives.
