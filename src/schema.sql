PRAGMA user_version = 1;
CREATE TABLE contents (id TEXT PRIMARY KEY, size INTEGER NOT NULL CHECK(size>=0));
CREATE TABLE xorbs (id TEXT PRIMARY KEY, size INTEGER NOT NULL CHECK(size>0));
CREATE TABLE xorb_chunks (
  xorb TEXT NOT NULL REFERENCES xorbs(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL, hash TEXT NOT NULL, len INTEGER NOT NULL,
  offset INTEGER NOT NULL, stored INTEGER NOT NULL,
  PRIMARY KEY(xorb,idx)
);
CREATE TABLE chunks (
  hash TEXT PRIMARY KEY, xorb TEXT NOT NULL, idx INTEGER NOT NULL,
  FOREIGN KEY(xorb,idx) REFERENCES xorb_chunks(xorb,idx)
);
CREATE INDEX chunk_locations ON chunks(xorb,idx);
CREATE TABLE file_chunks (
  content TEXT NOT NULL REFERENCES contents(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL, offset INTEGER NOT NULL, len INTEGER NOT NULL,
  hash TEXT NOT NULL REFERENCES chunks(hash), PRIMARY KEY(content,idx)
);
CREATE INDEX file_ranges ON file_chunks(content,offset);
CREATE INDEX chunk_references ON file_chunks(hash);
CREATE TABLE inodes (
  ino INTEGER PRIMARY KEY AUTOINCREMENT, kind INTEGER NOT NULL CHECK(kind IN (1,2,3)),
  mode INTEGER NOT NULL, uid INTEGER NOT NULL, gid INTEGER NOT NULL,
  size INTEGER NOT NULL CHECK(size>=0), atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL,
  content TEXT REFERENCES contents(id), target BLOB
);
CREATE TABLE entries (
  parent INTEGER NOT NULL REFERENCES inodes(ino), name BLOB NOT NULL,
  ino INTEGER NOT NULL REFERENCES inodes(ino), PRIMARY KEY(parent,name)
);
CREATE INDEX inode_links ON entries(ino);
CREATE TABLE snapshots (name TEXT PRIMARY KEY, created INTEGER NOT NULL);
CREATE TABLE snapshot_nodes (
  snapshot TEXT NOT NULL REFERENCES snapshots(name) ON DELETE CASCADE,
  ino INTEGER NOT NULL, kind INTEGER NOT NULL, mode INTEGER NOT NULL, uid INTEGER NOT NULL, gid INTEGER NOT NULL,
  size INTEGER NOT NULL, atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL,
  content TEXT REFERENCES contents(id), target BLOB, PRIMARY KEY(snapshot,ino)
);
CREATE TABLE snapshot_entries (
  snapshot TEXT NOT NULL REFERENCES snapshots(name) ON DELETE CASCADE,
  parent INTEGER NOT NULL, name BLOB NOT NULL, ino INTEGER NOT NULL, PRIMARY KEY(snapshot,parent,name)
);
