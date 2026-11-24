---
description: How to join, check and leave a Trio room from this session
disable-model-invocation: true
---
Tell the user, briefly, how Trio attached mode works in this session. Do not run any commands.

- Type `trio` as the whole message to join the nearest room (the one at or above this folder, or the only room Trio knows). Type `trio <room>` to pick one by name or path.
- `trio status` shows the room and unread count. `trio off` leaves.
- These are answered locally by the Trio hook; the model never sees them.
- While joined, each message you send brings the room lines you have not seen, and each final reply is posted to the room. Mentions of this agent in the room wait for your next message here; Trio never resumes this session from outside.
- Open the room itself with `trio open <room-dir>`.
