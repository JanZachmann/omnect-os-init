# bmap test data

`wic.bmap` is real `bmaptool create` output, and `wic.xz` is the image it
describes. The image is sparse: 20 blocks of 4096 bytes plus a partial block of
100 bytes, with data only in blocks 0-1, 5, 9-11 and 20.

Made with:

```sh
truncate -s $((20 * 4096 + 100)) wic
# <block>:<fill byte in octal>
for fill in 0:021 1:042 5:125 9:231 10:252 11:273; do
    printf '%4096s' '' | tr ' ' "\\${fill#*:}" |
        dd of=wic bs=4096 seek="${fill%:*}" conv=notrunc status=none
done
# block 20 holds the bytes 0 to 99
printf "$(printf '\\%03o' $(seq 0 99))" |
    dd of=wic bs=4096 seek=20 conv=notrunc status=none
bmaptool create wic -o wic.bmap
xz -T0 -k wic
```

The sha256 of the decoded image is
`f41a80f9f783ec8915b3420421664c25511937a60fbf9d9001b9b6ebbb53a2b7`.
