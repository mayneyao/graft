---
title: Graft Identifier (GID)
description: Graft Identifiers (GIDs) are 16 byte IDs used to globally identify Graft objects.
---

Graft uses a 16 byte identifier called a Graft Identifier (GID) to identify Segments, Clients, and Volumes. GIDs are similar to UUIDs and ULIDs with a Graft specific prefix and different canonical encoding.

- **128-bit compatibility** (same size as UUID)
- **Up to 2^72 unique GIDs per millisecond**
- **Lexicographically sortable** by creation time!
- **Canonically encoded as a 22-character string**, compared to the 36-character UUID
- **Case sensitive**
- **No special characters** (fully URL safe)
- **Creation time is embedded**: newer GIDs sort after older ones

GIDs have the following layout:

```
 0               1               2               3
 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|     prefix    |                   timestamp                   |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                   timestamp                   |     random    |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             random                            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             random                            |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

Every GID has a 1 byte prefix which encodes its type. There are currently three known GID types: Volume, Segment, and Client. The prefix may contain other types or namespace metadata in the future. The highest bit of the prefix is always set to ensure that GIDs bs58 serialize to exactly 22 bytes.

Following the prefix is a 48 bit timestamp encoding milliseconds since the unix epoch and stored in network byte order (MSB first).

Finally there are 72 bits of random noise allowing up to `2^72` GIDs to be generated per millisecond.

GIDs are canonically serialized into 22 bytes using the bs58 algorithm with the Bitcoin alphabet:

```
123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz
```
