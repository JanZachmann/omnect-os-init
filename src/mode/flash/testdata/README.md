# bmap test data

`wic.bmap` is real `bmaptool create` output, and `wic.xz` is the image it
describes. The image is sparse: 20 blocks of 4096 bytes plus a partial block of
100 bytes, with data only in blocks 0-1, 5, 9-11 and 20.

Made with:

```sh
truncate -s $((20 * 4096 + 100)) wic
# write data into the blocks above, leave the rest as holes
bmaptool create wic -o wic.bmap
xz -k wic
```

The sha256 of the decoded image is in the tests that use it.
