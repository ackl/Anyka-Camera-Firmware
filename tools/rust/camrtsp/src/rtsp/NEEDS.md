# rtsp/: interface changes needed outside this directory

The RTSP side asks for these but does not make them itself. None blocks the
server: it builds and passes its tests without them.

1. **Keyframe on join: `VideoSource::request_keyframe(&self)`** (source.rs,
   venc.rs). A new session waits for the next IDR before it sends anything,
   because a P-frame with no reference only decodes to garbage. On the real
   encoder that is one GOP, about 52 frames or 3.4 s (measured in
   `work/rtsp-rust-notes.md`), and ffmpeg logged 41–48 frames
   "skipped waiting for a keyframe" on every connect against the replay. The
   stock server starts within about 100 ms, so it forces an IDR. A
   default-no-op trait method, which the venc pump turns into the MPI's
   request-IDR call, would let `session::Sender` ask for one on PLAY and on
   resume after PAUSE. The other way to fix it, a GOP cache, costs up to one
   GOP of memory per source and a latency burst, so I did not build it.

2. **Explicit end-of-stream on `VideoSource`** (source.rs). The only way to
   tell that a source has finished (a replay without `loop`) is that
   `Hub::next()` returns `None` immediately instead of waiting out its
   timeout. `session::video_pump` treats a `None` back in under 20 ms as the
   end, then sends each session an RTCP BYE so ffmpeg exits cleanly. An
   `fn ended(&self) -> bool { false }` on the trait, which Hub would answer
   from `running`, would replace that guess.

3. **`tools/remote-build.sh`: create the remote directory first.** The first
   build from a new worktree fails with
   `rsync: mkdir ".../src/anyka-build/<hash>/camrtsp" failed: No such file or directory`
   because rsync does not create the hashed parent directory. Adding
   `ssh build-01 "mkdir -p $remote"` before the rsync fixes it. I worked around
   it once by hand with `ssh build-01 mkdir -p src/anyka-build/2f18d3d39a71fd59`.
