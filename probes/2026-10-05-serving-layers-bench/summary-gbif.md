
#### First open at zoom 0, a viewer new to the server

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | counts | 154 / 190 | 156 / 160 |
| 100% | first points | 896 / 1,101 | 607 / 752 |
| 100% | last byte of points | 1,091 / 1,475 | 10,997 / 13,406 |
| 100% | last byte of layers | 11,704 / 12,824 | 21,261 / 21,434 |
| 100% | points beside layer work | 1,091 / 1,475 | 10,997 / 13,406 |
| 100% | settled | 39,245 / 41,441 | 45,109 / 46,019 |
| 100% | MB received | 6.54 | 21.08 |
| 25% | counts | 57 / 151 | 48 / 62 |
| 25% | first points | 519 / 642 | 472 / 496 |
| 25% | last byte of points | 7,864 / 9,796 | 8,924 / 9,843 |
| 25% | last byte of layers | 9,061 / 10,801 | 10,761 / 13,134 |
| 25% | points beside layer work | 7,864 / 9,796 | 8,895 / 9,808 |
| 25% | settled | 24,106 / 33,506 | 25,963 / 26,488 |
| 25% | MB received | 8.40 | 11.43 |
| 1% | counts | 265 / 283 | 326 / 338 |
| 1% | first points | 810 / 861 | 1,012 / 1,016 |
| 1% | last byte of points | 4,650 / 5,712 | 4,948 / 5,798 |
| 1% | last byte of layers | 6,460 / 6,799 | 8,670 / 10,016 |
| 1% | points beside layer work | 4,650 / 5,712 | 4,948 / 5,798 |
| 1% | settled | 11,660 / 15,344 | 14,727 / 15,001 |
| 1% | MB received | 1.43 | 2.09 |

#### The same viewer opening again

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | counts | 291 / 336 | 260 / 331 |
| 100% | first points | 1,144 / 1,320 | 741 / 1,273 |
| 100% | last byte of points | 1,310 / 1,389 | 2,437 / 3,142 |
| 100% | last byte of layers | 1,653 / 2,095 | 3,497 / 3,646 |
| 100% | points beside layer work | 1,310 / 1,389 | 1,782 / 2,295 |
| 100% | settled | 1,666 / 2,109 | 3,497 / 3,646 |
| 100% | MB received | 6.54 | 21.08 |
| 25% | counts | 186 / 333 | 187 / 209 |
| 25% | first points | 760 / 930 | 568 / 943 |
| 25% | last byte of points | 3,529 / 3,981 | 2,300 / 4,961 |
| 25% | last byte of layers | 1,850 / 2,162 | 2,669 / 3,511 |
| 25% | points beside layer work | 1,980 / 2,402 | 1,230 / 4,393 |
| 25% | settled | 3,549 / 4,030 | 2,669 / 4,961 |
| 25% | MB received | 8.41 | 11.44 |
| 1% | counts | 87 / 144 | 49 / 56 |
| 1% | first points | 107 / 168 | 65 / 73 |
| 1% | last byte of points | 435 / 505 | 336 / 509 |
| 1% | last byte of layers | 370 / 397 | 2,497 / 2,579 |
| 1% | points beside layer work | 382 / 400 | 336 / 509 |
| 1% | settled | 435 / 505 | 2,497 / 2,579 |
| 1% | MB received | 1.43 | 2.09 |

#### Reopen: the server restarted over its cache

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | counts | 170 / 317 | 158 / 160 |
| 100% | first points | 485 / 925 | 421 / 464 |
| 100% | last byte of points | 571 / 1,116 | 1,627 / 1,680 |
| 100% | last byte of layers | 971 / 1,595 | 1,977 / 2,173 |
| 100% | points beside layer work | 571 / 1,116 | 484 / 531 |
| 100% | settled | 1,513 / 2,518 | 1,977 / 2,173 |
| 100% | MB received | 6.54 | 21.08 |
| 25% | counts | 102 / 110 | 48 / 71 |
| 25% | first points | 769 / 1,008 | 403 / 551 |
| 25% | last byte of points | 6,440 / 6,941 | 2,963 / 4,671 |
| 25% | last byte of layers | 1,267 / 1,802 | 2,399 / 3,464 |
| 25% | points beside layer work | 2,520 / 3,895 | 2,478 / 3,722 |
| 25% | settled | 6,545 / 6,962 | 2,963 / 4,671 |
| 25% | MB received | 8.42 | 11.44 |
| 1% | counts | 142 / 322 | 162 / 306 |
| 1% | first points | 783 / 997 | 727 / 963 |
| 1% | last byte of points | 2,439 / 2,901 | 2,104 / 4,449 |
| 1% | last byte of layers | 2,383 / 2,933 | 3,798 / 6,024 |
| 1% | points beside layer work | 2,387 / 2,901 | 2,104 / 4,434 |
| 1% | settled | 2,629 / 3,194 | 3,798 / 6,024 |
| 1% | MB received | 1.43 | 2.09 |

#### Zoom 9, genus and species, two regions per run

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | first points | 152 / 449 | 151 / 463 |
| 100% | last byte of points | 834 / 1,163 | 1,339 / 1,497 |
| 100% | last byte of layers | 1,139 / 2,442 | 5,800 / 9,134 |
| 100% | points beside layer work | 834 / 1,163 | 1,339 / 1,497 |
| 100% | settled | 1,167 / 2,465 | 5,800 / 9,134 |
| 100% | MB received | 12.84 | 22.95 |
| 100% | artifact requests | 4 | 12 |
| 25% | first points | 130 / 191 | 91 / 178 |
| 25% | last byte of points | 307 / 357 | 396 / 587 |
| 25% | last byte of layers | 1,145 / 1,267 | 4,773 / 6,176 |
| 25% | points beside layer work | 307 / 357 | 396 / 587 |
| 25% | settled | 1,173 / 1,287 | 4,773 / 6,176 |
| 25% | MB received | 12.42 | 19.84 |
| 25% | artifact requests | 4 | 12 |
| 1% | first points | 26 / 57 | 25 / 32 |
| 1% | last byte of points | 460 / 518 | 493 / 698 |
| 1% | last byte of layers | 638 / 1,893 | 3,789 / 4,999 |
| 1% | points beside layer work | 460 / 518 | 481 / 698 |
| 1% | settled | 663 / 1,918 | 3,789 / 4,999 |
| 1% | MB received | 8.52 | 9.07 |
| 1% | artifact requests | 4 | 12 |

#### Pan at zoom 9

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | first points | 7 / 56 | 10 / 197 |
| 100% | last byte of points | 24 / 143 | 608 / 1,756 |
| 100% | last byte of layers | 344 / 386 | 3,477 / 3,824 |
| 100% | points beside layer work | — | 278 / 846 |
| 100% | settled | 354 / 386 | 3,477 / 3,824 |
| 100% | MB received | 1.21 | 23.56 |
| 100% | artifact requests | 4 | 12 |
| 25% | first points | 32 / 176 | 10 / 46 |
| 25% | last byte of points | 85 / 405 | 558 / 602 |
| 25% | last byte of layers | 568 / 1,098 | 3,726 / 4,101 |
| 25% | points beside layer work | 405 / 405 | 343 / 567 |
| 25% | settled | 568 / 1,116 | 3,726 / 4,101 |
| 25% | MB received | 3.18 | 25.46 |
| 25% | artifact requests | 4 | 12 |
| 1% | first points | 14 / 38 | 14 / 18 |
| 1% | last byte of points | 69 / 130 | 435 / 509 |
| 1% | last byte of layers | 283 / 325 | 3,478 / 3,624 |
| 1% | points beside layer work | — | 261 / 331 |
| 1% | settled | 287 / 325 | 3,478 / 3,624 |
| 1% | MB received | 1.62 | 7.53 |
| 1% | artifact requests | 4 | 12 |

#### Pan at zoom 6

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | first points | 29 / 200 | 85 / 190 |
| 100% | last byte of points | 346 / 897 | 804 / 2,310 |
| 100% | last byte of layers | 313 / 735 | 3,596 / 3,895 |
| 100% | points beside layer work | 346 / 897 | 670 / 2,310 |
| 100% | settled | 346 / 920 | 3,596 / 3,895 |
| 100% | MB received | 1.56 | 14.77 |
| 100% | artifact requests | 4 | 12 |
| 25% | first points | 121 / 329 | 9 / 345 |
| 25% | last byte of points | 341 / 951 | 553 / 578 |
| 25% | last byte of layers | 313 / 489 | 3,493 / 3,563 |
| 25% | points beside layer work | 341 / 608 | 333 / 345 |
| 25% | settled | 341 / 951 | 3,493 / 3,563 |
| 25% | MB received | 0.23 | 0.92 |
| 25% | artifact requests | 4 | 12 |
| 1% | first points | 27 / 61 | 12 / 18 |
| 1% | last byte of points | 102 / 1,545 | 554 / 610 |
| 1% | last byte of layers | 293 / 487 | 3,412 / 3,814 |
| 1% | points beside layer work | 237 / 266 | 306 / 322 |
| 1% | settled | 487 / 1,545 | 3,412 / 3,814 |
| 1% | MB received | 3.14 | 13.13 |
| 1% | artifact requests | 4 | 12 |

#### Zoom out to the world from zoom 14

| viewer | measure | prefetch off middle / max ms | prefetch on middle / max ms |
|---|---|---:|---:|
| 100% | first points | — | 327 / 381 |
| 100% | last byte of points | — | 585 / 769 |
| 100% | last byte of layers | — | — |
| 100% | points beside layer work | — | — |
| 100% | settled | 289 / 295 | 297 / 769 |
| 100% | MB received | 0.00 | 0.00 |
| 100% | artifact requests | 0 | 0 |
| 25% | first points | — | 353 / 1,467 |
| 25% | last byte of points | — | 353 / 1,467 |
| 25% | last byte of layers | — | — |
| 25% | points beside layer work | — | — |
| 25% | settled | 286 / 287 | 289 / 1,467 |
| 25% | MB received | 0.00 | 0.00 |
| 25% | artifact requests | 0 | 0 |
| 1% | first points | — | 292 / 294 |
| 1% | last byte of points | — | 292 / 294 |
| 1% | last byte of layers | — | — |
| 1% | points beside layer work | — | — |
| 1% | settled | 272 / 289 | 271 / 294 |
| 1% | MB received | 0.00 | 0.00 |
| 1% | artifact requests | 0 | 0 |

#### Status figures (`masked_count_cache`) after each phase

| run | phase | fills | loads | not_admitted | reserve_spent | labels_rows_read | exact | hits | misses | disk bytes |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| gbif-new-off-1 | run | 9 | 0 | 0 | 0 | 0 | 0 | 294 | 9 | 339,891,297 |
| gbif-new-off-1 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 33 | 9 | 339,891,297 |
| gbif-new-off-2 | run | 9 | 0 | 0 | 0 | 0 | 0 | 306 | 9 | 339,891,297 |
| gbif-new-off-2 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 42 | 9 | 339,891,297 |
| gbif-new-on-1 | run | 9 | 0 | 0 | 0 | 0 | 0 | 439 | 9 | 339,891,297 |
| gbif-new-on-1 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 36 | 9 | 339,891,297 |
| gbif-new-on-2 | run | 9 | 0 | 0 | 0 | 0 | 0 | 430 | 9 | 339,891,297 |
| gbif-new-on-2 | reopen | 0 | 9 | 0 | 0 | 0 | 0 | 33 | 9 | 339,891,297 |

#### Requests and bytes over the whole run, by kind

| run | kind | requests | MB | whole-level |
|---|---|---:|---:|---:|
| gbif-new-off-1 | /session/authorise | 9 | 0.0 | 0 |
| gbif-new-off-1 | aggregate | 69 | 0.1 | 0 |
| gbif-new-off-1 | artifacts | 63 | 16.2 | 0 |
| gbif-new-off-1 | artifacts-by-id | 90 | 4.5 | 0 |
| gbif-new-off-1 | counts | 9 | 6.8 | 0 |
| gbif-new-off-1 | marks | 176 | 320.6 | 0 |
| gbif-new-off-1 | meta | 9 | 0.0 | 0 |
| gbif-new-off-2 | /session/authorise | 9 | 0.0 | 0 |
| gbif-new-off-2 | aggregate | 69 | 0.1 | 0 |
| gbif-new-off-2 | artifacts | 63 | 16.2 | 0 |
| gbif-new-off-2 | artifacts-by-id | 97 | 4.5 | 0 |
| gbif-new-off-2 | counts | 9 | 6.8 | 0 |
| gbif-new-off-2 | marks | 176 | 320.6 | 0 |
| gbif-new-off-2 | meta | 9 | 0.0 | 0 |
| gbif-new-on-1 | /session/authorise | 9 | 0.0 | 0 |
| gbif-new-on-1 | aggregate | 69 | 0.1 | 0 |
| gbif-new-on-1 | artifacts | 178 | 30.8 | 0 |
| gbif-new-on-1 | artifacts-by-id | 93 | 4.3 | 0 |
| gbif-new-on-1 | counts | 9 | 6.8 | 0 |
| gbif-new-on-1 | marks | 262 | 653.7 | 0 |
| gbif-new-on-1 | meta | 9 | 0.0 | 0 |
| gbif-new-on-2 | /session/authorise | 9 | 0.0 | 0 |
| gbif-new-on-2 | aggregate | 69 | 0.1 | 0 |
| gbif-new-on-2 | artifacts | 178 | 30.8 | 0 |
| gbif-new-on-2 | artifacts-by-id | 89 | 4.3 | 0 |
| gbif-new-on-2 | counts | 9 | 6.8 | 0 |
| gbif-new-on-2 | marks | 262 | 654.9 | 0 |
| gbif-new-on-2 | meta | 9 | 0.0 | 0 |

#### Whole-level artifact requests


#### What each prefetch-on run sent beyond its prefetch-off pair

| run | kind | requests | MB | slowest ms |
|---|---|---:|---:|---:|
| gbif-new-on-1 | artifacts | 142 | 19.0 | 4,919 |
| gbif-new-on-1 | artifacts-by-id | 69 | 3.2 | 627 |
| gbif-new-on-1 | marks | 113 | 412.1 | 9,124 |
| gbif-new-on-2 | artifacts | 142 | 19.0 | 4,969 |
| gbif-new-on-2 | artifacts-by-id | 69 | 3.2 | 534 |
| gbif-new-on-2 | marks | 113 | 413.3 | 11,309 |
