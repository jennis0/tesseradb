
#### First open at zoom 0, a viewer new to the server

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | counts | 12 / 13 | 12 / 32 |
| 100% | first points | 46 / 46 | 72 / 128 |
| 100% | last byte of points | 252 / 255 | 213 / 313 |
| 100% | last byte of layers | 257 / 267 | 288 / 320 |
| 100% | points beside layer work | — | 313 / 313 |
| 100% | settled | 333 / 345 | 1,048 / 1,496 |
| 100% | MB received | 8.24 | 6.60 |
| 25% | counts | 12 / 12 | 13 / 14 |
| 25% | first points | 23 / 24 | 30 / 35 |
| 25% | last byte of points | 264 / 284 | 186 / 411 |
| 25% | last byte of layers | 210 / 212 | 256 / 285 |
| 25% | points beside layer work | 209 / 213 | 290 / 290 |
| 25% | settled | 264 / 284 | 752 / 939 |
| 25% | MB received | 4.03 | 2.88 |
| 1% | counts | 24 / 24 | 39 / 63 |
| 1% | first points | 37 / 38 | 60 / 110 |
| 1% | last byte of points | 157 / 161 | 203 / 266 |
| 1% | last byte of layers | 210 / 210 | 246 / 266 |
| 1% | points beside layer work | — | 266 / 266 |
| 1% | settled | 260 / 261 | 727 / 803 |
| 1% | MB received | 0.95 | 0.69 |

#### The same viewer opening again

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | counts | 11 / 11 | 13 / 30 |
| 100% | first points | 42 / 43 | 46 / 95 |
| 100% | last byte of points | 249 / 250 | 219 / 341 |
| 100% | last byte of layers | 253 / 257 | 226 / 267 |
| 100% | points beside layer work | 247 / 250 | 219 / 219 |
| 100% | settled | 316 / 318 | 417 / 560 |
| 100% | MB received | 8.24 | 6.60 |
| 25% | counts | 10 / 10 | 12 / 14 |
| 25% | first points | 18 / 19 | 22 / 23 |
| 25% | last byte of points | 147 / 171 | 119 / 196 |
| 25% | last byte of layers | 207 / 207 | 212 / 222 |
| 25% | points beside layer work | — | — |
| 25% | settled | 254 / 254 | 254 / 257 |
| 25% | MB received | 4.03 | 2.88 |
| 1% | counts | 10 / 11 | 13 / 18 |
| 1% | first points | 18 / 18 | 22 / 28 |
| 1% | last byte of points | 95 / 96 | 177 / 230 |
| 1% | last byte of layers | 206 / 208 | 212 / 223 |
| 1% | points beside layer work | — | 230 / 230 |
| 1% | settled | 253 / 254 | 255 / 314 |
| 1% | MB received | 0.95 | 0.69 |

#### Reopen: the server restarted over its cache

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | counts | 18 / 51 | 13 / 19 |
| 100% | first points | 77 / 136 | 50 / 88 |
| 100% | last byte of points | 620 / 625 | 179 / 392 |
| 100% | last byte of layers | 242 / 245 | 212 / 222 |
| 100% | points beside layer work | 235 / 235 | — |
| 100% | settled | 620 / 625 | 410 / 703 |
| 100% | MB received | 8.24 | 6.60 |
| 25% | counts | 11 / 43 | 10 / 10 |
| 25% | first points | 22 / 66 | 20 / 20 |
| 25% | last byte of points | 188 / 603 | 137 / 141 |
| 25% | last byte of layers | 208 / 236 | 213 / 213 |
| 25% | points beside layer work | — | — |
| 25% | settled | 254 / 603 | 317 / 323 |
| 25% | MB received | 4.03 | 2.88 |
| 1% | counts | 20 / 40 | 22 / 23 |
| 1% | first points | 33 / 76 | 36 / 40 |
| 1% | last byte of points | 146 / 410 | 113 / 118 |
| 1% | last byte of layers | 210 / 247 | 222 / 223 |
| 1% | points beside layer work | 243 / 243 | — |
| 1% | settled | 260 / 410 | 312 / 315 |
| 1% | MB received | 0.95 | 0.69 |

#### Zoom 9, genus and species, two regions per run

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | 16 / 37 | 12 / 40 |
| 100% | last byte of points | 109 / 612 | 74 / 162 |
| 100% | last byte of layers | 454 / 461 | 222 / 325 |
| 100% | points beside layer work | 455 / 461 | — |
| 100% | settled | 256 / 612 | 255 / 325 |
| 100% | MB received | 6.40 | 6.10 |
| 100% | artifact requests | 3 | 6 |
| 25% | first points | 11 / 23 | 16 / 23 |
| 25% | last byte of points | 377 / 448 | 63 / 387 |
| 25% | last byte of layers | 373 / 385 | 228 / 265 |
| 25% | points beside layer work | 372 / 385 | — |
| 25% | settled | 377 / 448 | 313 / 387 |
| 25% | MB received | 9.26 | 3.95 |
| 25% | artifact requests | 3 | 6 |
| 1% | first points | — | — |
| 1% | last byte of points | — | — |
| 1% | last byte of layers | 452 / 517 | 222 / 359 |
| 1% | points beside layer work | — | — |
| 1% | settled | 255 / 517 | 287 / 393 |
| 1% | MB received | 0.00 | 0.31 |
| 1% | artifact requests | 3 | 6 |

#### Pan at zoom 9

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | 6 / 20 | 5 / 11 |
| 100% | last byte of points | 35 / 202 | 19 / 60 |
| 100% | last byte of layers | — | 215 / 229 |
| 100% | points beside layer work | — | — |
| 100% | settled | 256 / 441 | 255 / 343 |
| 100% | MB received | 1.41 | 1.21 |
| 100% | artifact requests | 0 | 6 |
| 25% | first points | 10 / 12 | 8 / 17 |
| 25% | last byte of points | 17 / 62 | 15 / 80 |
| 25% | last byte of layers | — | 210 / 249 |
| 25% | points beside layer work | — | — |
| 25% | settled | 253 / 281 | 259 / 314 |
| 25% | MB received | 0.02 | 0.08 |
| 25% | artifact requests | 0 | 6 |
| 1% | first points | — | — |
| 1% | last byte of points | — | — |
| 1% | last byte of layers | — | 220 / 223 |
| 1% | points beside layer work | — | — |
| 1% | settled | 254 / 257 | 273 / 288 |
| 1% | MB received | 0.00 | 0.23 |
| 1% | artifact requests | 0 | 6 |

#### Pan at zoom 6

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | 10 / 13 | 5 / 31 |
| 100% | last byte of points | 65 / 219 | 18 / 283 |
| 100% | last byte of layers | — | 207 / 380 |
| 100% | points beside layer work | — | 283 / 283 |
| 100% | settled | 261 / 432 | 269 / 452 |
| 100% | MB received | 0.56 | 0.27 |
| 100% | artifact requests | 0 | 6 |
| 25% | first points | 11 / 11 | 9 / 9 |
| 25% | last byte of points | 611 / 662 | 469 / 607 |
| 25% | last byte of layers | — | 209 / 244 |
| 25% | points beside layer work | — | 219 / 219 |
| 25% | settled | 255 / 662 | 254 / 607 |
| 25% | MB received | 0.00 | 0.04 |
| 25% | artifact requests | 0 | 6 |
| 1% | first points | 9 / 9 | 12 / 14 |
| 1% | last byte of points | 45 / 621 | 98 / 1,704 |
| 1% | last byte of layers | — | 210 / 277 |
| 1% | points beside layer work | — | 206 / 222 |
| 1% | settled | 257 / 621 | 304 / 1,704 |
| 1% | MB received | 0.00 | 0.03 |
| 1% | artifact requests | 0 | 6 |

#### Zoom out to the world from zoom 14

| viewer | measure | old p50 / max ms | new p50 / max ms |
|---|---|---:|---:|
| 100% | first points | — | — |
| 100% | last byte of points | — | — |
| 100% | last byte of layers | — | — |
| 100% | points beside layer work | — | — |
| 100% | settled | 260 / 315 | 260 / 262 |
| 100% | MB received | 0.00 | 0.00 |
| 100% | artifact requests | 0 | 0 |
| 25% | first points | — | — |
| 25% | last byte of points | — | — |
| 25% | last byte of layers | — | — |
| 25% | points beside layer work | — | — |
| 25% | settled | 256 / 259 | 256 / 262 |
| 25% | MB received | 0.00 | 0.00 |
| 25% | artifact requests | 0 | 0 |
| 1% | first points | — | — |
| 1% | last byte of points | — | — |
| 1% | last byte of layers | — | — |
| 1% | points beside layer work | — | — |
| 1% | settled | 255 / 264 | 257 / 259 |
| 1% | MB received | 0.00 | 0.00 |
| 1% | artifact requests | 0 | 0 |

#### Status figures (`masked_count_cache`) after each phase

| run | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | hits | misses | disk bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| old-off-1 | run | 9 | 0 | 0 | 0 | 0 | 0 | 202 | 9 | 44,501,549 |
| old-off-1 | reopen | 0 | 3 | 0 | 0 | 0 | 0 | 25 | 3 | 44,501,549 |
| old-off-2 | run | 9 | 0 | 0 | 0 | 0 | 0 | 202 | 9 | 44,501,549 |
| old-off-2 | reopen | 0 | 3 | 0 | 0 | 0 | 0 | 25 | 3 | 44,501,549 |
| old-off-3 | run | 9 | 0 | 0 | 0 | 0 | 0 | 202 | 9 | 44,501,549 |
| old-off-3 | reopen | 0 | 3 | 0 | 0 | 0 | 0 | 25 | 3 | 44,501,549 |
| new-off-1 | run | 9 | 0 | 0 | 0 | 0 | 0 | 230 | 9 | 44,501,549 |
| new-off-1 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 3 | 9 | 44,501,549 |
| new-off-2 | run | 9 | 0 | 0 | 0 | 0 | 0 | 239 | 9 | 44,501,549 |
| new-off-2 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 6 | 9 | 44,501,549 |
| new-off-3 | run | 9 | 0 | 0 | 0 | 0 | 0 | 218 | 9 | 44,501,549 |
| new-off-3 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 6 | 9 | 44,501,549 |

#### Requests and bytes over the whole run, by kind

| run | kind | requests | MB | idle | whole-level |
|---|---|---:|---:|---:|---:|
| old-off-1 | /session/authorise | 9 | 0.0 | 0 | 0 |
| old-off-1 | aggregate | 69 | 0.1 | 0 | 0 |
| old-off-1 | artifacts | 15 | 7.3 | 0 | 0 |
| old-off-1 | counts | 9 | 2.2 | 0 | 0 |
| old-off-1 | marks | 197 | 201.4 | 0 | 0 |
| old-off-1 | meta | 9 | 0.0 | 0 | 0 |
| old-off-1 | promotion | 6 | 37.4 | 6 | 6 |
| old-off-2 | /session/authorise | 9 | 0.0 | 0 | 0 |
| old-off-2 | aggregate | 69 | 0.1 | 0 | 0 |
| old-off-2 | artifacts | 15 | 7.3 | 0 | 0 |
| old-off-2 | counts | 9 | 2.2 | 0 | 0 |
| old-off-2 | marks | 197 | 201.4 | 0 | 0 |
| old-off-2 | meta | 9 | 0.0 | 0 | 0 |
| old-off-2 | promotion | 6 | 37.4 | 6 | 6 |
| old-off-3 | /session/authorise | 9 | 0.0 | 0 | 0 |
| old-off-3 | aggregate | 69 | 0.1 | 0 | 0 |
| old-off-3 | artifacts | 15 | 7.3 | 0 | 0 |
| old-off-3 | counts | 9 | 2.2 | 0 | 0 |
| old-off-3 | marks | 197 | 201.4 | 0 | 0 |
| old-off-3 | meta | 9 | 0.0 | 0 | 0 |
| old-off-3 | promotion | 6 | 37.4 | 6 | 6 |
| new-off-1 | /session/authorise | 9 | 0.0 | 0 | 0 |
| new-off-1 | aggregate | 69 | 0.1 | 0 | 0 |
| new-off-1 | artifacts | 62 | 11.8 | 0 | 9 |
| new-off-1 | artifacts-by-id | 60 | 1.7 | 0 | 0 |
| new-off-1 | counts | 9 | 2.2 | 0 | 0 |
| new-off-1 | marks | 197 | 171.2 | 0 | 0 |
| new-off-1 | meta | 9 | 0.0 | 0 | 0 |
| new-off-2 | /session/authorise | 9 | 0.0 | 0 | 0 |
| new-off-2 | aggregate | 69 | 0.1 | 0 | 0 |
| new-off-2 | artifacts | 62 | 11.8 | 0 | 9 |
| new-off-2 | artifacts-by-id | 63 | 1.7 | 0 | 0 |
| new-off-2 | counts | 9 | 2.2 | 0 | 0 |
| new-off-2 | marks | 197 | 171.2 | 0 | 0 |
| new-off-2 | meta | 9 | 0.0 | 0 | 0 |
| new-off-3 | /session/authorise | 9 | 0.0 | 0 | 0 |
| new-off-3 | aggregate | 69 | 0.1 | 0 | 0 |
| new-off-3 | artifacts | 62 | 11.8 | 0 | 9 |
| new-off-3 | artifacts-by-id | 56 | 1.7 | 0 | 0 |
| new-off-3 | counts | 9 | 2.2 | 0 | 0 |
| new-off-3 | marks | 197 | 171.2 | 0 | 0 |
| new-off-3 | meta | 9 | 0.0 | 0 | 0 |

#### Whole-level artifact requests

- old-off-1 1%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 0.4 MB, 7 ms
- old-off-1 1%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 0.5 MB, 8 ms
- old-off-1 25%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 2.3 MB, 28 ms
- old-off-1 25%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 6.6 MB, 86 ms
- old-off-1 100%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 5.6 MB, 75 ms
- old-off-1 100%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 22.0 MB, 526 ms
- old-off-2 1%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 0.4 MB, 6 ms
- old-off-2 1%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 0.5 MB, 10 ms
- old-off-2 25%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 2.3 MB, 28 ms
- old-off-2 25%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 6.6 MB, 90 ms
- old-off-2 100%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 5.6 MB, 77 ms
- old-off-2 100%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 22.0 MB, 344 ms
- old-off-3 1%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 0.4 MB, 7 ms
- old-off-3 1%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 0.5 MB, 8 ms
- old-off-3 25%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 2.3 MB, 27 ms
- old-off-3 25%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 6.6 MB, 89 ms
- old-off-3 100%/map/1 promotion (idle): depth 0, tiles bbox, levels [1], 5.6 MB, 70 ms
- old-off-3 100%/map/3 promotion (idle): depth 0, tiles bbox, levels [2], 22.0 MB, 327 ms
- new-off-1 1%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 37 ms
- new-off-1 1%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 17 ms
- new-off-1 25%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 74 ms
- new-off-1 25%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 11 ms
- new-off-1 100%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 75 ms
- new-off-1 100%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 8 ms
- new-off-1 1%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 15 ms
- new-off-1 25%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 11 ms
- new-off-1 100%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 14 ms
- new-off-2 1%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 31 ms
- new-off-2 1%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 10 ms
- new-off-2 25%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 53 ms
- new-off-2 25%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 14 ms
- new-off-2 100%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 114 ms
- new-off-2 100%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 17 ms
- new-off-2 1%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 13 ms
- new-off-2 25%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 11 ms
- new-off-2 100%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 10 ms
- new-off-3 1%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 22 ms
- new-off-3 1%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 9 ms
- new-off-3 25%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 29 ms
- new-off-3 25%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 10 ms
- new-off-3 100%/first artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 50 ms
- new-off-3 100%/again artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 9 ms
- new-off-3 1%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 13 ms
- new-off-3 25%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 11 ms
- new-off-3 100%/reopen artifacts: depth 2, tiles 16, levels [0], 0.1 MB, 9 ms
