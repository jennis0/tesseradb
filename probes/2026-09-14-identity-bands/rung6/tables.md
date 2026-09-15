## Whole map, k = 30 (the battery's request)

| principal | reference cold | reference hot | reference read, hot | band cold | band hot | band read, cold |
|---|---|---|---|---|---|---|
| 1% | 3.32 s | 0.05 s | 0.0 GB | 0.345 s | 0.011 s | 0.05 GB |
| 5% | 5.45 s | 0.23 s | 0.0 GB | 0.023 s | 0.012 s | 0.00 GB |
| 10% | 5.63 s | 0.51 s | 0.0 GB | 0.043 s | 0.028 s | 0.00 GB |
| 25% | 10.88 s | 6.79 s | 6.8 GB | 0.036 s | 0.024 s | 0.00 GB |
| 50% | 14.70 s | 11.31 s | 12.0 GB | 0.055 s | 0.044 s | 0.00 GB |
| 100% | 12.90 s | 11.91 s | 21.1 GB | 0.026 s | 0.017 s | 0.00 GB |

## Whole map, 2,000,000-mark budget (depth 9)

| principal | served | reference cold | reference hot | reference CPU, hot | reference read, hot | band cold | band hot | band CPU, hot | band read, cold | identity reads |
|---|---|---|---|---|---|---|---|---|---|---|
| 1% | 43,649 | 10.7 s | 0.4 s | 0.5 s | 0.0 GB | 9.05 s | 0.062 s | 0.067 s | 3.11 GB | 7,292 |
| 5% | 58,655 | 20.1 s | 0.5 s | 0.7 s | 0.0 GB | 1.69 s | 0.068 s | 0.067 s | 2.02 GB | 10,849 |
| 10% | 279,754 | 28.2 s | 1.7 s | 2.3 s | 0.8 GB | 1.84 s | 0.125 s | 0.131 s | 1.93 GB | 107,225 |
| 25% | 283,072 | 50.8 s | 22.5 s | 17.5 s | 32.8 GB | 2.38 s | 0.155 s | 0.169 s | 2.18 GB | 64,740 |
| 50% | 324,814 | 90.6 s | 52.3 s | 42.9 s | 71.4 GB | 1.49 s | 0.155 s | 0.171 s | 1.27 GB | 57,195 |
| 100% | 1,712,874 | 120.5 s | 81.3 s | 78.0 s | 118.3 GB | 1.35 s | 0.194 s | 0.204 s | 0.65 GB | 662,564 |

## The shape across zooms, 100% and 50% principals, budget cases, hot

| principal | view | depth | band | served | reference wall | reference CPU | band wall | band CPU | source |
|---|---|---|---|---|---|---|---|---|---|
| 100% | whole map | 9 | 10 | 1,712,874 | 81.31 s | 77.97 s | 0.194 s | 0.204 s | list |
| 100% | densest depth-2 tile | 11 | 7 | 4,038,307 | 20.96 s | 27.98 s | 0.386 s | 0.406 s | list |
| 100% | densest depth-4 tile | 13 | 5 | 11,902,722 | 4.14 s | 7.73 s | 0.697 s | 0.735 s | list |
| 100% | densest depth-6 tile | 15 | 3 | 7,974,232 | 1.61 s | 2.75 s | 0.801 s | 0.860 s | leading-zero column |
| 100% | densest depth-8 tile | 16 | 2 | 1,912,712 | 0.41 s | 0.60 s | 0.158 s | 0.168 s | leading-zero column |
| 50% | whole map | 9 | 12 | 324,814 | 52.30 s | 42.92 s | 0.155 s | 0.171 s | list |
| 50% | densest depth-2 tile | 11 | 9 | 1,142,062 | 13.02 s | 17.06 s | 0.118 s | 0.124 s | list |
| 50% | densest depth-4 tile | 13 | 6 | 492,742 | 0.43 s | 0.71 s | 0.051 s | 0.054 s | list |
| 50% | densest depth-6 tile | 15 | 3 | 1,083,903 | 0.33 s | 0.52 s | 0.111 s | 0.116 s | leading-zero column |
| 50% | densest depth-8 tile | 16 | 2 | 3 | 0.02 s | 0.03 s | 0.002 s | 0.002 s | leading-zero column |

## The band route's own reads at the whole-map budget, hot

| principal | tiles | candidates | list bytes | leading-zero bytes | identity reads | floor tiles | settled by band / list / column | search split (candidates / count / floor) | position |
|---|---|---|---|---|---|---|---|---|---|
| 1% | 2,942 | 68,277 | 370.5 MB | 0.0 MB | 7,292 | 2,335 | 38 / 535 / 1,762 | 10 / 1 / 44 ms | 1.2 ms |
| 5% | 4,033 | 85,495 | 300.2 MB | 0.0 MB | 10,849 | 3,414 | 26 / 915 / 2,473 | 12 / 2 / 44 ms | 2.2 ms |
| 10% | 27,217 | 683,334 | 320.3 MB | 0.0 MB | 107,225 | 23,137 | 741 / 8,999 / 13,397 | 39 / 12 / 56 ms | 3.5 ms |
| 25% | 21,328 | 426,889 | 763.6 MB | 0.0 MB | 64,740 | 17,798 | 227 / 8,082 / 9,489 | 29 / 8 / 99 ms | 4.8 ms |
| 50% | 23,070 | 427,162 | 465.7 MB | 0.0 MB | 57,195 | 18,660 | 160 / 7,217 / 11,283 | 51 / 8 / 75 ms | 4.6 ms |
| 100% | 154,835 | 3,411,766 | 69.0 MB | 0.0 MB | 662,564 | 132,024 | 2,544 / 65,954 / 63,526 | 31 / 59 / 70 ms | 9.3 ms |

## The render at the whole-map budget: two columns against the cut index (first run)

| principal | rows | columns, cold | columns, hot | columns pages (modelled) | cut index, cold | cut index, hot | cut index pages (modelled) |
|---|---|---|---|---|---|---|---|
| 1% | 43,649 | 0.00 s | 0.00 s | 0.13 GB | 0.005 s | 0.003 s | 0.030 GB |
| 5% | 58,655 | 0.00 s | 0.00 s | 0.26 GB | 0.007 s | 0.007 s | 0.049 GB |
| 10% | 279,754 | 0.01 s | 0.00 s | 0.77 GB | 0.025 s | 0.024 s | 0.088 GB |
| 25% | 283,072 | 2.17 s | 6.41 s | 1.10 GB | 0.076 s | 0.060 s | 0.128 GB |
| 50% | 324,814 | 12.50 s | 10.73 s | 1.33 GB | 0.124 s | 0.038 s | 0.183 GB |
| 100% | 1,712,874 | 17.71 s | 19.04 s | 5.28 GB | 0.207 s | 0.139 s | 0.335 GB |

## Quantised counts against the exact one, whole-map budget (first run)

| principal | exact | band j | ratio | band j+1 | ratio | fp16 | tie reads |
|---|---|---|---|---|---|---|---|
| 1% | 47,144 | 68,277 | 1.448 | 34,196 | 0.725 | 47,144 | 26 |
| 5% | 64,571 | 85,495 | 1.324 | 42,674 | 0.661 | 64,571 | 41 |
| 10% | 435,536 | 683,334 | 1.569 | 341,340 | 0.784 | 435,536 | 337 |
| 25% | 341,831 | 426,889 | 1.249 | 213,578 | 0.625 | 341,831 | 223 |
| 50% | 369,582 | 427,162 | 1.156 | 213,731 | 0.578 | 369,582 | 235 |
| 100% | 2,476,009 | 3,411,766 | 1.378 | 1,706,342 | 0.689 | 2,476,009 | 1,671 |

## Session costs (final pass)

| principal | visible | projection build wall | projection CPU | occupancy ladder to depth 16 |
|---|---|---|---|---|
| 1% | 34,956,939 | 6.1 s | 8.4 s | 0.03 s |
| 5% | 174,787,222 | 9.2 s | 13.2 s | 0.07 s |
| 10% | 349,571,416 | 15.2 s | 21.0 s | 0.16 s |
| 25% | 873,932,430 | 20.8 s | 40.3 s | 0.33 s |
| 50% | 1,747,866,322 | 30.7 s | 76.1 s | 1.17 s |
| 100% | 3,495,729,729 | 26.8 s | 119.8 s | 1.99 s |
