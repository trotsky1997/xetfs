#ifndef XETFS_MEMORY_H
#define XETFS_MEMORY_H
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
/* A process owns the repository until close; do not use a handle after fork.
 * Buffers and callbacks must remain live/stable through each operation.
 * Failed reads may leave an explicitly supplied destination partially written. */
typedef struct MemoryHandle XetfsMemory;
typedef struct { uint64_t size; char content[65]; } XetfsMemoryInfo;
typedef int64_t (*XetfsMemoryReader)(void *context, uint8_t *target, size_t capacity);
uint32_t xetfs_memory_abi(void);
const char *xetfs_memory_error(void); /* thread-local; copy before the next error */
int32_t xetfs_memory_open(const char *root, bool create, XetfsMemory **out);
/* Clone owns a reference to the same repository lock/cache; close independently. */
int32_t xetfs_memory_clone(XetfsMemory *handle, XetfsMemory **out);
void xetfs_memory_close(XetfsMemory *handle);
int32_t xetfs_memory_put(XetfsMemory *, const char *path, const uint8_t *, size_t, XetfsMemoryInfo *);
int32_t xetfs_memory_put_reader(XetfsMemory *, const char *path, XetfsMemoryReader, void *, uint64_t, XetfsMemoryInfo *);
int32_t xetfs_memory_stat(XetfsMemory *, const char *path, XetfsMemoryInfo *);
int32_t xetfs_memory_publish(XetfsMemory *, const char *source, const char *destination);
/* Encoded native Xorbs for cuFile DMA. Import validates and publishes a file
 * in repository tmp; caller must finish DMA and close writers before import. */
int32_t xetfs_memory_xorb_stat(XetfsMemory *, const char *xorb_id, XetfsMemoryInfo *);
int32_t xetfs_memory_import_xorb(XetfsMemory *, const char *source, const char *xorb_id, XetfsMemoryInfo *);
/* Reads immutable content IDs returned by stat/put; repeated ranges stay pinned. */
int32_t xetfs_memory_read_into(XetfsMemory *, const char *content_id, uint64_t offset, uint8_t *target, size_t capacity, uint64_t *written);
#ifdef __cplusplus
}
#endif
#endif
