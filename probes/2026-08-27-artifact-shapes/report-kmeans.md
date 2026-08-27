# kmeans: 64 artifacts, 13658 … 73360 members

## Families, whole layer

| family | rings (median / max) | vertices | wire bytes | area / wrap (median) | fill (median) | members outside | precision (median) |
|---|---|---|---|---|---|---|---|
| convex wrap | 1 / 1 | 1,587 | 12,696 | 1.000 | 0.963 | 0 | 1.000 |
| alpha shape (as built) | 1 / 1 | 4,707 | 37,656 | 0.941 | 0.994 | 0 | 1.000 |
| alpha-complex | 1 / 51 | 6,349 | 51,316 | 0.963 | 1.000 | 137 | 1.000 |
| chi-shape | 1 / 1 | 4,860 | 38,880 | 0.920 | 0.997 | 0 | 1.000 |
| chi-shape, Douglas-Peucker at alpha/2 | 1 / 1 | 933 | 7,464 | 0.841 | 0.991 | 220,809 | 1.000 |

## Fill, by family — the share of the drawn shape within alpha of a member

| family | min | p25 | median | p75 | artifacts under 0.5 |
|---|---|---|---|---|---|
| convex wrap | 0.010 | 0.923 | 0.963 | 0.983 | 4 / 64 |
| alpha shape (as built) | 0.011 | 0.980 | 0.994 | 0.997 | 4 / 64 |
| alpha-complex | 1.000 | 1.000 | 1.000 | 1.000 | 0 / 64 |
| chi-shape | 0.059 | 0.994 | 0.997 | 0.998 | 3 / 64 |
| chi-shape, Douglas-Peucker at alpha/2 | 0.046 | 0.976 | 0.991 | 0.998 | 3 / 64 |

## Multi-modality at the same alpha

- artifacts with 2+ components holding >= 5% of members:  **4 / 64**
- artifacts with 2+ components holding >= 10% of members: **4 / 64**
- artifacts with 2+ components holding >= 1% of members:  **5 / 64**
- artifacts with 3+ components holding >= 5% of members:  **3 / 64**
- largest component's share of members: median 1.000, min 0.260
- members outside their artifact's largest component: 45,413 of 2,422,484 member rows (1.9%)

### The multi-modal artifacts, and what each family does with them

| artifact | members | components >=5% | largest share | dig fill | alpha-complex rings | chi fill |
|---|---|---|---|---|---|---|
| km-000010 | 37,785 | 2 | 0.69 | 0.360 | 14 | 0.898 |
| km-000034 | 27,674 | 3 | 0.61 | 0.058 | 39 | 0.428 |
| km-000008 | 16,042 | 3 | 0.36 | 0.067 | 27 | 0.342 |
| km-000041 | 14,289 | 7 | 0.26 | 0.011 | 51 | 0.059 |

## The dig's own limits

- at the 64-vertex budget: **34 / 64**
- finishing with a live edge still above alpha (the budget, not alpha, stopped it): **34 / 64**
- digs refused for want of a candidate or for simplicity, whole layer: **14**

## By membership size

| members | artifacts | convex wrap fill / vertices | alpha shape (as built) fill / vertices | alpha-complex fill / vertices | chi-shape fill / vertices | chi-shape, Douglas-Peucker at alpha/2 fill / vertices |
|---|---|---|---|---|---|---|
| 10,000 – 50,000 | 53 | 0.962 / 22 | 0.993 / 81 | 1.000 / 65 | 0.996 / 44 | 0.988 / 6 |
| 50,000 – 200,000 | 11 | 0.969 / 34 | 0.996 / 94 | 1.000 / 149 | 0.997 / 78 | 0.996 / 9 |

## Precision: does a shape swallow other artifacts' points?

- convex wrap: median 1.000, p10 1.000, min 1.000, artifacts under 0.9: 0 / 64
- alpha shape (as built): median 1.000, p10 1.000, min 1.000, artifacts under 0.9: 0 / 64
- alpha-complex: median 1.000, p10 1.000, min 1.000, artifacts under 0.9: 0 / 64
- chi-shape: median 1.000, p10 1.000, min 1.000, artifacts under 0.9: 0 / 64
- chi-shape, Douglas-Peucker at alpha/2: median 1.000, p10 1.000, min 1.000, artifacts under 0.9: 0 / 64

## Alpha-complex: what it costs to be honest

- members left outside the shape, whole layer: **137** of 2,422,484
- artifacts leaving at least one member outside: 4 / 64
- rings: median 1, max 51
- holes: total 14, artifacts with a hole 5 / 64
- wire bytes, whole layer: 51,316 against the dig's 37,656
