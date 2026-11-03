PRAGMA user_version = 4;
CREATE TABLE repository_info (id INTEGER PRIMARY KEY CHECK(id=1), uuid BLOB NOT NULL CHECK(length(uuid)=16));
INSERT INTO repository_info(id,uuid) VALUES(1,randomblob(16));
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
  size INTEGER NOT NULL CHECK(size>=0), atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL, birthtime INTEGER NOT NULL,
  flags INTEGER NOT NULL DEFAULT 0 CHECK(flags IN (0,32768)),
  content TEXT REFERENCES contents(id), target BLOB
);
CREATE TABLE entries (
  parent INTEGER NOT NULL REFERENCES inodes(ino), name BLOB NOT NULL,
  ino INTEGER NOT NULL REFERENCES inodes(ino), PRIMARY KEY(parent,name)
);
CREATE INDEX inode_links ON entries(ino);
CREATE TABLE inode_xattrs (
  ino INTEGER NOT NULL REFERENCES inodes(ino) ON DELETE CASCADE,
  name BLOB NOT NULL CHECK(length(name)>0 AND length(name)<=127 AND instr(name,x'00')=0),
  value BLOB NOT NULL, PRIMARY KEY(ino,name)
);
CREATE TABLE snapshots (name TEXT PRIMARY KEY, created INTEGER NOT NULL);
CREATE TABLE snapshot_nodes (
  snapshot TEXT NOT NULL REFERENCES snapshots(name) ON DELETE CASCADE,
  ino INTEGER NOT NULL, kind INTEGER NOT NULL, mode INTEGER NOT NULL, uid INTEGER NOT NULL, gid INTEGER NOT NULL,
  size INTEGER NOT NULL, atime INTEGER NOT NULL, mtime INTEGER NOT NULL, ctime INTEGER NOT NULL, birthtime INTEGER NOT NULL,
  flags INTEGER NOT NULL CHECK(flags IN (0,32768)),
  content TEXT REFERENCES contents(id), target BLOB, PRIMARY KEY(snapshot,ino)
);
CREATE TABLE snapshot_entries (
  snapshot TEXT NOT NULL REFERENCES snapshots(name) ON DELETE CASCADE,
  parent INTEGER NOT NULL, name BLOB NOT NULL, ino INTEGER NOT NULL, PRIMARY KEY(snapshot,parent,name)
);
CREATE TABLE snapshot_xattrs (
  snapshot TEXT NOT NULL, ino INTEGER NOT NULL,
  name BLOB NOT NULL CHECK(length(name)>0 AND length(name)<=127 AND instr(name,x'00')=0),
  value BLOB NOT NULL,
  PRIMARY KEY(snapshot,ino,name),
  FOREIGN KEY(snapshot,ino) REFERENCES snapshot_nodes(snapshot,ino) ON DELETE CASCADE
);
