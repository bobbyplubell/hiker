# Charts

Hiker renders ` ```chart ` blocks as data charts, in place, the same way it
renders math and Mermaid diagrams. You describe *what* you want — a mark (bar,
line, …), which columns map to which axes — and Hiker resolves and draws it with
its own pure-Rust engine (no browser, no JavaScript, no network).

A chart block has two parts separated by a line that is exactly `---`:

1. a small **config** in YAML (the mark + channel mappings + options), then
2. the **data** as inline CSV.

```
```
​chart
mark: bar
x: region
y: sales
---
region,sales
North,120
South,90
```
```

The data can also live in a separate file instead of inline — see
[External data](#external-data) below.

**Reveal works exactly like the other widgets** (see
[Widgets & diagrams](widgets.md)): with your cursor away from the block the chart
is shown; move the cursor inside (or select it) and the source returns for
editing. **Clicking a rendered chart opens it in the chart builder** — a visual
editor with live preview — where you can retarget columns, restyle, and **save
back to the note**.

Everything below is a live example. Open this note in Hiker and each block
renders.

---

## Bar

A bar chart maps a category column to `x` and a value column to `y`.

```chart
mark: bar
x: region
y: sales
config:
  title: Sales by region
---
region,sales
North,120
South,90
East,140
West,75
```

### Grouped bars (multiple series)

Give `y` a **list** of columns to draw one series per column, side by side.

```chart
mark: bar
x: quarter
y: [revenue, profit]
config:
  legend: true
---
quarter,revenue,profit
Q1,100,20
Q2,140,35
Q3,180,55
Q4,160,40
```

### Stacked bars

Set `config.stack: true` to stack the series on a cumulative baseline instead.

```chart
mark: bar
x: quarter
y: [revenue, profit]
config:
  stack: true
---
quarter,revenue,profit
Q1,100,20
Q2,140,35
Q3,180,55
Q4,160,40
```

### Horizontal bars

`config.orientation: horizontal` swaps the axes.

```chart
mark: bar
x: language
y: users
config:
  orientation: horizontal
---
language,users
Rust,140
Python,320
Go,90
TypeScript,210
```

---

## Line

A line chart connects the `y` values across an ordered `x`. Pass a `y` list for
multiple lines.

```chart
mark: line
x: month
y: [revenue, profit]
config:
  title: Monthly performance
  legend: true
---
month,revenue,profit
Jan,100,20
Feb,140,35
Mar,180,55
Apr,160,40
May,210,70
Jun,250,95
```

A **step** interpolation (`config.interpolate: step`) holds each value until the
next sample — useful for state or count-over-time data.

```chart
mark: line
x: hour
y: active
config:
  interpolate: step
---
hour,active
0,3
1,3
2,5
3,8
4,8
5,4
```

---

## Area

Area is a line with the region beneath it filled. `config.fill_opacity`
controls the fill; `stack: true` stacks multiple areas.

```chart
mark: area
x: month
y: [organic, paid]
config:
  stack: true
  fill_opacity: 0.6
  legend: true
---
month,organic,paid
Jan,40,10
Feb,55,18
Mar,62,30
Apr,70,28
May,85,45
```

---

## Point (scatter & bubble)

A point chart plots `x` against `y` as dots. Bind a `size` column to scale each
dot into a **bubble**, and a `color` column to tint by category.

```chart
mark: point
x: weight
y: mpg
size: power
color: kind
config:
  title: Weight vs. efficiency
  legend: true
---
weight,mpg,power,kind
1.2,52,80,compact
1.6,44,110,compact
2.1,33,150,sedan
2.8,26,210,sedan
3.4,19,300,truck
3.9,16,360,truck
```

---

## Histogram

A histogram bins a single quantitative column and plots the counts. `config.bins`
sets the bucket count (the engine picks a sensible default otherwise).

```chart
mark: histogram
x: score
config:
  bins: 8
  title: Score distribution
---
score
55
61
63
66
67
68
70
71
72
72
73
74
74
75
76
78
79
81
83
88
91
```

---

## Arc (pie & donut)

An arc chart sums a `theta` value per `color` category into wedges. With no
`inner_radius` it's a pie; set `config.inner_radius` (a `0.0`–`0.9` fraction) for
a donut.

```chart
mark: arc
color: source
theta: visits
config:
  title: Traffic sources
  legend: true
  stack: false
  show_grid: true
---
source,visits
Search,540
Direct,310
Social,220
Referral,130
```

Donut variant — same data, with a hole cut out:

```chart
mark: arc
theta: visits
color: source
config:
  inner_radius: 0.55
  legend: true
---
source,visits
Search,540
Direct,310
Social,220
Referral,130
```

---

## Long-format data (split by a column)

When your series live in **rows** (a "metric" column) rather than columns, bind
`y` to the value column and `color` to the splitting column — one series is drawn
per distinct category.

```chart
mark: line
x: day
y: value
color: metric
config:
  legend: true
---
day,metric,value
Mon,signups,12
Mon,logins,80
Tue,signups,18
Tue,logins,95
Wed,signups,9
Wed,logins,88
Thu,signups,22
Thu,logins,120
```

---

## Table

The `table` mark renders the data as a formatted grid rather than a plot. Use
`columns` to pick and order which columns show; `config.transpose: true` flips
fields and records.

```chart
mark: table
columns: [name, role, commits]
config:
  title: Contributors
---
name,role,commits,email
Ada,Maintainer,412,ada@example.com
Linus,Reviewer,288,linus@example.com
Grace,Contributor,77,grace@example.com
```

---

## Styling & options

Presentation lives under `config`. A few of the common knobs, shown together:

```chart
mark: bar
x: team
y: points
config:
  title: League standings
  x_title: Team
  y_title: Points
  legend: false
  show_grid: true
  palette: ["#2563eb", "#16a34a", "#dc2626", "#d97706"]
---
team,points
Falcons,68
Wolves,61
Sharks,55
Bears,49
```

| Option | What it does |
|---|---|
| `title`, `x_title`, `y_title` | Chart and axis titles |
| `legend` | Show/hide the series legend |
| `show_grid` | Interior gridlines (cartesian marks) |
| `stack` | Stack bar/area series on a cumulative baseline |
| `orientation` | `vertical` (default) or `horizontal` |
| `interpolate` | `linear` (default) or `step` (line) |
| `fill_opacity`, `line_width`, `point_size` | Per-mark sizing |
| `bins` | Histogram bucket count |
| `inner_radius` | Donut hole (arc), `0.0`–`0.9` |
| `palette` | Override series colors (`#rrggbb` list) |
| `x_scale`, `y_scale` | Axis transform — see below |

### Axis scales

An axis can use a `log` or `sqrt` transform, an explicit domain, and a
zero-baseline toggle:

```chart
mark: point
x: population
'y': gdp
config:
  title: GDP vs. population (log–log)
  legend: true
  stack: false
  show_grid: true
  x_scale:
    kind: log
    zero: false
  y_scale:
    kind: log
    zero: false
---
population,gdp
1,2
10,18
100,210
1000,2600
10000,31000
```

---

## External data

Instead of an inline `---` data section, a block can reference a CSV file with
`data:`. The path resolves like a link — relative to the note first, then by the
same rules wikilinks use — and is sandboxed to the vault.

```
​```chart
mark: line
x: date
y: close
data: data/prices.csv
​```
```

Opening a `.csv` file directly also drops you straight into the chart builder
over that file's data, with a **Copy as block** action that emits either a
self-contained block (config + inline CSV, like the examples above) or a
`data:`-reference block like this one.

---

## Tips

- **No `---`, no data.** A block with config but no `---` section (and no
  `data:`) has nothing to plot. If a chart won't render, check that the separator
  line is exactly `---` and that the CSV header names match your channel
  mappings.
- **Click to edit.** The fastest way to build a chart is to start with any block
  above, click the rendered chart to open the builder, and adjust visually — then
  **Save to note**.
- **Column names with spaces or commas** are fine in the data (CSV quoting is
  handled), but in the config quote them, e.g. `y: "Net revenue"`.
