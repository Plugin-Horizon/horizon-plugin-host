# Gaps: hooks Horizon does not offer yet

What the host cannot do faithfully, and the hook that would close it. Nothing here is faked in the host.

## 1. Chat decorate (no server-side hook that replaces the rendered line)

The host's `player_chat` hook can cancel a chat line, but a plugin cannot change how a signed line is rendered.
A rewrite (`rewrite_chat`) therefore cancels the signed line and broadcasts `<name> text` as an unsigned system line.

Needed: a server-side chat hook that gets the rendered message and may replace it while the line stays a player's chat
line (signed, or at least shown with the player's name and chat type).

## 2. Level of the player in chat and hits

`PlayerChatEvent` and `PlayerHurtByPlayerEvent` carry no operator level, and their hooks take no `Host`, so the host
cannot call `player_permission` or `has_permission` there. Plugins see `Player.permission = 0` in `chat`, and
`player_damage` gets no attacker level at all.

Needed: the sender's level in those events, or a `Host` argument on `player_chat` and `player_hurt_by_player`.

## Closed since the last revision (for reference)

- Level of a joining or leaving player: `Host::player_permission` (feature `hooks-all`).
- Commands added or removed by a load, reload or unload: `Host::refresh_commands` (feature `hooks-all`).
