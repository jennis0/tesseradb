# hdbscan: 197 artifacts, 6146 … 2422484 members

## Families, whole layer

| family | rings (median / max) | vertices | wire bytes | area / wrap (median) | fill (median) | members outside | precision (median) |
|---|---|---|---|---|---|---|---|
| convex wrap | 1 / 1 | 3,278 | 26,224 | 1.000 | 0.857 | 0 | 0.937 |
| alpha shape (as built) | 1 / 1 | 12,388 | 99,104 | 0.870 | 0.976 | 0 | 0.965 |
| alpha-complex | 1 / 10 | 10,073 | 80,648 | 0.857 | 1.000 | 24 | 0.972 |
| chi-shape | 1 / 1 | 9,441 | 75,528 | 0.825 | 0.992 | 0 | 0.977 |
| chi-shape, Douglas-Peucker at alpha/2 | 1 / 1 | 1,494 | 11,952 | 0.737 | 0.957 | 712,003 | 0.972 |

## Fill, by family — the share of the drawn shape within alpha of a member

| family | min | p25 | median | p75 | artifacts under 0.5 |
|---|---|---|---|---|---|
| convex wrap | 0.068 | 0.760 | 0.857 | 0.932 | 4 / 197 |
| alpha shape (as built) | 0.089 | 0.917 | 0.976 | 0.989 | 3 / 197 |
| alpha-complex | 1.000 | 1.000 | 1.000 | 1.000 | 0 / 197 |
| chi-shape | 0.618 | 0.985 | 0.992 | 0.995 | 0 / 197 |
| chi-shape, Douglas-Peucker at alpha/2 | 0.451 | 0.916 | 0.957 | 0.984 | 1 / 197 |

## Multi-modality at the same alpha

- artifacts with 2+ components holding >= 5% of members:  **3 / 197**
- artifacts with 2+ components holding >= 10% of members: **1 / 197**
- artifacts with 2+ components holding >= 1% of members:  **3 / 197**
- artifacts with 3+ components holding >= 5% of members:  **1 / 197**
- largest component's share of members: median 1.000, min 0.465
- members outside their artifact's largest component: 8,321 of 12,808,677 member rows (0.1%)

### The multi-modal artifacts, and what each family does with them

| artifact | members | components >=5% | largest share | dig fill | alpha-complex rings | chi fill |
|---|---|---|---|---|---|---|
| hdb-2422544 | 16,929 | 2 | 0.91 | 0.817 | 1 | 0.995 |
| hdb-2422491 | 11,280 | 3 | 0.46 | 0.089 | 10 | 0.618 |
| hdb-2422523 | 7,237 | 2 | 0.92 | 0.446 | 6 | 0.897 |

## The dig's own limits

- at the 64-vertex budget: **108 / 197**
- finishing with a live edge still above alpha (the budget, not alpha, stopped it): **108 / 197**
- digs refused for want of a candidate or for simplicity, whole layer: **13**
- artifacts of >= 200,000 members: 11, median fill 0.796, median area/wrap 0.934

## By membership size

| members | artifacts | convex wrap fill / vertices | alpha shape (as built) fill / vertices | alpha-complex fill / vertices | chi-shape fill / vertices | chi-shape, Douglas-Peucker at alpha/2 fill / vertices |
|---|---|---|---|---|---|---|
| 0 – 10,000 | 60 | 0.872 / 15 | 0.982 / 52 | 1.000 / 33 | 0.990 / 25 | 0.952 / 4 |
| 10,000 – 50,000 | 103 | 0.861 / 16 | 0.977 / 79 | 1.000 / 42 | 0.991 / 35 | 0.951 / 5 |
| 50,000 – 200,000 | 23 | 0.834 / 21 | 0.893 / 85 | 1.000 / 75 | 0.995 / 68 | 0.953 / 8 |
| 200,000+ | 11 | 0.742 / 23 | 0.796 / 87 | 1.000 / 102 | 0.996 / 106 | 0.973 / 14 |

## Precision: does a shape swallow other artifacts' points?

- convex wrap: median 0.937, p10 0.780, min 0.599, artifacts under 0.9: 81 / 197
- alpha shape (as built): median 0.965, p10 0.839, min 0.658, artifacts under 0.9: 50 / 197
- alpha-complex: median 0.972, p10 0.874, min 0.784, artifacts under 0.9: 35 / 197
- chi-shape: median 0.977, p10 0.884, min 0.794, artifacts under 0.9: 27 / 197
- chi-shape, Douglas-Peucker at alpha/2: median 0.972, p10 0.866, min 0.702, artifacts under 0.9: 33 / 197

## Alpha-complex: what it costs to be honest

- members left outside the shape, whole layer: **24** of 12,808,677
- artifacts leaving at least one member outside: 2 / 197
- rings: median 1, max 10
- holes: total 3, artifacts with a hole 2 / 197
- wire bytes, whole layer: 80,648 against the dig's 99,104
