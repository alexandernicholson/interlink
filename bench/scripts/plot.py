"""Pure, standard-library SVG rendering for measured mesh performance."""

from dataclasses import dataclass
from html import escape
import math

__all__ = ["render"]

_MESHES = ("interlink", "linkerd", "istio-ambient")
_DASHES = ("", "8 5", "2 4")
_METRICS = ("rps", "p99_ms", "payload_mib_s", "cpu_millicores", "rss_mib")


def _number(value, label, *, positive=False):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{label} must be a number")
    if value < 0 or value > 1e15 or not math.isfinite(value) or (positive and value == 0):
        raise ValueError(f"{label} is outside the finite plotting range")
    return float(value)


@dataclass(frozen=True)
class _Point:
    mesh: str
    workload: str
    concurrency: int
    rps: float
    p99_ms: float
    payload_mib_s: float
    cpu_millicores: float
    rss_mib: float
    eligible: bool


def _point(point, workloads):
    if point.mesh not in _MESHES or point.workload not in workloads:
        raise ValueError("unexpected mesh or workload in plot")
    concurrency = _number(point.concurrency, "concurrency", positive=True)
    if not concurrency.is_integer():
        raise ValueError("concurrency must be an integer")
    if not isinstance(point.eligible, bool):
        raise ValueError("point eligibility must be boolean")
    metrics = {key: _number(getattr(point, key), key, positive=key == "rps")
               for key in _METRICS}
    return _Point(point.mesh, point.workload, int(concurrency), **metrics,
                  eligible=point.eligible)


def _value(value):
    if value == 0:
        return "0"
    if value < 0.01 or value >= 1e7:
        return f"{value:.3g}"
    return f"{value:,.2f}".rstrip("0").rstrip(".")


def _tick(value):
    if value >= 1e6:
        return f"{value / 1e6:g}M"
    if value >= 1000:
        return f"{value / 1000:g}k"
    return f"{value:g}"


def _linear(values):
    maximum = max(max(values) * 1.06, 1e-6)
    magnitude = 10 ** math.floor(math.log10(maximum / 4))
    step = next(factor * magnitude for factor in (1, 2, 2.5, 5, 10)
                if factor * magnitude >= maximum / 4)
    count = math.ceil(maximum / step)
    upper = count * step
    return lambda value: value / upper, [(i / count, _tick(i * step))
                                         for i in range(count + 1)]


def _latency(values):
    if min(values) <= 0:
        axis, ticks = _linear(values)
        return axis, ticks, "p99 latency (ms)"
    low = math.floor(math.log10(min(values)))
    high = max(low + 1, math.ceil(math.log10(max(values))))
    stride = max(1, math.ceil((high - low) / 5))
    exponents = list(range(low, high, stride)) + [high]
    ticks = [((power - low) / (high - low),
              _tick(10.0 ** power) if -3 <= power <= 6 else f"1e{power}")
             for power in exponents]
    return (lambda value: (math.log10(value) - low) / (high - low),
            ticks, "p99 latency (ms, log scale)")


def _text(parts, x, y, text, *, css="", anchor="start", size=None):
    attrs = f' class="{css}"' if css else ""
    if size is not None:
        attrs += f' font-size="{size}"'
    parts.append(f'<text x="{x:.2f}" y="{y:.2f}" text-anchor="{anchor}"{attrs}>'
                 f'{escape(str(text))}</text>')


def _line(parts, x1, y1, x2, y2, css, extra=""):
    parts.append(f'<line x1="{x1:.2f}" y1="{y1:.2f}" '
                 f'x2="{x2:.2f}" y2="{y2:.2f}" class="{css}"{extra}/>')


def _marker(parts, mesh_index, x, y, size, *, control=False, title=None,
            measurement=None):
    measurement_class = f" {measurement}-point" if measurement else ""
    attrs = f' class="mesh{mesh_index} marker{" control" if control else ""}{measurement_class}"'
    if measurement:
        attrs += (f' data-mesh="{_MESHES[mesh_index]}"'
                  f' data-x="{x:.2f}" data-y="{y:.2f}"')
    parts.append(f'<g{attrs}>')
    if title:
        parts.append(f'<title>{escape(title)}</title>')
    if control:
        parts.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{size + 3:.2f}" '
                     'stroke-width="1"/>')
    if mesh_index == 0:
        parts.append(f'<circle cx="{x:.2f}" cy="{y:.2f}" r="{size:.2f}"/>')
    elif mesh_index == 1:
        parts.append(f'<rect x="{x - size:.2f}" y="{y - size:.2f}" '
                     f'width="{2 * size:.2f}" height="{2 * size:.2f}"/>')
    else:
        parts.append(f'<path d="M {x:.2f},{y - size:.2f} '
                     f'L {x + size:.2f},{y + size:.2f} '
                     f'L {x - size:.2f},{y + size:.2f} Z"/>')
    parts.append('</g>')


def render(capacity_points, shared_points, host, target_rps=10000) -> str:
    """Return a deterministic 1000 × 990 SVG, preserving supplied capacity order.

    ``shared_points`` contains one eligible, actually measured ``shared`` point per
    mesh. ``target_rps`` is their common *offered* rate, never their plotted x value.
    Statistical/cohort validation belongs to the caller. Numeric values must be
    finite, nonnegative and at most 1e15; rates and concurrency must be positive.
    Only host CPU count and RAM size are displayed, not identifying host metadata.
    """
    target = _number(target_rps, "target_rps", positive=True)
    cpus = _number(host["logical_cpus"], "host logical_cpus", positive=True)
    memory = _number(host["memory_bytes"], "host memory_bytes", positive=True)
    if not cpus.is_integer():
        raise ValueError("host logical_cpus must be an integer")
    # Failed standard-client reads may retain negative byte sentinels. They are
    # valid boundary evidence, not coordinates for an error-free performance plot.
    capacity = [_point(point, ("rps", "bulk")) for point in capacity_points if point.eligible]
    shared = [_point(point, ("shared",)) for point in shared_points]
    if (len(shared) != len(_MESHES) or {point.mesh for point in shared} != set(_MESHES)
            or not all(point.eligible for point in shared)):
        raise ValueError("one eligible shared control is required per mesh")
    if len({point.concurrency for point in shared}) != 1:
        raise ValueError("shared controls must use common concurrency")
    controls = {point.mesh: point for point in shared}
    # Filtering retains the declared concurrency order, including overload reversals.
    groups = {(mesh, workload): [point for point in capacity
                                if point.mesh == mesh and point.workload == workload
                                and point.eligible]
              for mesh in _MESHES for workload in ("rps", "bulk")}
    if any(not points for points in groups.values()):
        raise ValueError("each mesh needs eligible RPS and bulk capacity measurements")
    common_low = max(min(point.rps for point in groups[mesh, "rps"]) for mesh in _MESHES)
    common_high = min(max(point.rps for point in groups[mesh, "rps"]) for mesh in _MESHES)
    rates = [point.rps for mesh in _MESHES for point in groups[mesh, "rps"]]
    x_rate, rate_ticks = _linear(rates + [point.rps for point in shared] + [target])
    concurrency = sorted({point.concurrency for mesh in _MESHES
                          for point in groups[mesh, "bulk"]})
    log_low = math.log2(concurrency[0])
    log_high = math.log2(concurrency[-1])
    if log_high == log_low:
        log_low -= 1
        log_high += 1
    x_bulk = lambda value: (math.log2(value) - log_low) / (log_high - log_low)
    # Tick thinning never inserts unmeasured concurrency values.
    stride = max(1, math.ceil(len(concurrency) / 7))
    tick_concurrency = concurrency[::stride]
    if tick_concurrency[-1] != concurrency[-1]:
        tick_concurrency.append(concurrency[-1])
    bulk_ticks = [(x_bulk(value), _tick(value)) for value in tick_concurrency]

    parts = [
        '<svg xmlns="http://www.w3.org/2000/svg" width="1000" height="990" '
        'viewBox="0 0 1000 990" role="img" aria-labelledby="plot-title plot-desc">',
        '<title id="plot-title">Measured service mesh echo performance</title>',
        '<desc id="plot-desc">Four panels show p99 latency, proxy RSS, proxy CPU, '
        'and bulk payload throughput. Small markers are eligible capacity measurements '
        'in declared concurrency order. Large ringed markers are separate measured '
        'shared-load controls at their actual achieved request rates.</desc>',
        '<style>'
        'svg{color:var(--fg,CanvasText);color-scheme:light dark;'
        'font-family:system-ui,-apple-system,sans-serif;font-size:14px}'
        'text{fill:currentColor}'
        '.muted{fill:var(--muted,currentColor)}'
        '.axis{stroke:currentColor;stroke-width:1;fill:none}'
        '.grid{stroke:var(--border,currentColor);stroke-opacity:.18;stroke-width:1}'
        '.shade{fill:var(--accent,currentColor);fill-opacity:.08}'
        '.target{stroke:currentColor;stroke-opacity:.65;stroke-dasharray:2 5;stroke-width:1.5}'
        '.mesh0{stroke:var(--c1,LinkText)}'
        '.mesh1{stroke:var(--c2,ActiveText)}'
        '.mesh2{stroke:var(--c3,VisitedText)}'
        '.curve{fill:none;stroke-width:2;stroke-linejoin:round}'
        '.marker{fill:none;stroke-width:1.5}'
        '.control{stroke-width:2.5}'
        '</style>',
    ]
    _text(parts, 24, 29, "Service mesh · measured echo performance", size=22)
    _text(parts, 24, 53,
          f"Same host · {int(cpus)} logical CPUs · {_value(memory / 2**30)} GiB RAM",
          css="muted")
    for index, mesh in enumerate(_MESHES):
        x = 24 + index * 215
        dash = f' stroke-dasharray="{_DASHES[index]}"' if _DASHES[index] else ""
        _line(parts, x, 80, x + 38, 80, f"mesh{index} curve", dash)
        _marker(parts, index, x + 19, 80, 3.5)
        _text(parts, x + 48, 85, mesh)
    _marker(parts, 0, 715, 80, 5, control=True)
    _text(parts, 733, 85, "Measured shared control")
    _text(parts, 24, 112,
          f"Shared controls: {_value(target)} RPS offered to each mesh · "
          f"concurrency {shared[0].concurrency} · 1 KiB request / response")
    common_label = (f"Common capacity range: {_value(common_low)}–{_value(common_high)} RPS (shaded)"
                    if common_low <= common_high else "No overlapping measured capacity RPS range")
    _text(parts, 24, 135, common_label, css="muted")

    panels = (
        (24, 154, "1 KiB · p99 latency", "p99_ms", "ms"),
        (516, 154, "1 KiB · proxy resident memory", "rss_mib", "MiB"),
        (24, 548, "1 KiB · proxy CPU", "cpu_millicores", "cores"),
        (516, 548, "1 MiB · bulk payload throughput", "payload_mib_s", "MiB/s"),
    )
    for panel_x, panel_y, title, metric, unit in panels:
        parts.append(f'<g id="panel-{metric}" class="panel" role="group" '
                     f'aria-label="{escape(title, quote=True)}">')
        bulk = metric == "payload_mib_s"
        workload = "bulk" if bulk else "rps"
        left, top, width, height = panel_x + 57, panel_y + 48, 386, 190
        right, bottom = left + width, top + height
        divisor = 1000 if metric == "cpu_millicores" else 1
        plotted = [point for mesh in _MESHES for point in groups[mesh, workload]]
        values = [getattr(point, metric) / divisor
                  for point in plotted + ([] if bulk else shared)]
        if metric == "p99_ms":
            y_axis, y_ticks, y_label = _latency(values)
        else:
            y_axis, y_ticks = _linear(values)
            y_label = {"rss_mib": "Proxy RSS (MiB)",
                       "cpu_millicores": "Proxy CPU (cores)",
                       "payload_mib_s": "Payload (MiB/s per direction)"}[metric]
        x_axis, x_ticks = (x_bulk, bulk_ticks) if bulk else (x_rate, rate_ticks)
        _text(parts, panel_x, panel_y + 17, title, size=17)
        _text(parts, left, top - 13, y_label, css="muted", size=13)
        if not bulk and common_low <= common_high:
            shade_x = left + width * x_rate(common_low)
            shade_width = width * (x_rate(common_high) - x_rate(common_low))
            parts.append(f'<rect class="shade" x="{shade_x:.2f}" y="{top:.2f}" '
                         f'width="{shade_width:.2f}" height="{height:.2f}"/>')
        for position, label in y_ticks:
            y = bottom - height * position
            _line(parts, left, y, right, y, "grid")
            _text(parts, left - 9, y + 5, label, anchor="end", size=13)
        for position, label in x_ticks:
            x = left + width * position
            _line(parts, x, top, x, bottom, "grid")
            _text(parts, x, bottom + 22, label, anchor="middle", size=13)
        _line(parts, left, top, left, bottom, "axis")
        _line(parts, left, bottom, right, bottom, "axis")
        if not bulk:
            target_x = left + width * x_rate(target)
            _line(parts, target_x, top, target_x, bottom, "target")
            _text(parts, right, top - 13, f"Dotted: {_tick(target)} offered",
                  anchor="end", size=12)
        _text(parts, (left + right) / 2, bottom + 44,
              "Concurrency (log scale)" if bulk else "Achieved request rate (RPS)",
              anchor="middle")
        for index, mesh in enumerate(_MESHES):
            points = groups[mesh, workload]
            coordinates = [(left + width * x_axis(point.concurrency if bulk else point.rps),
                            bottom - height * y_axis(getattr(point, metric) / divisor))
                           for point in points]
            coordinate_text = " ".join(f"{x:.2f},{y:.2f}" for x, y in coordinates)
            dash = f' stroke-dasharray="{_DASHES[index]}"' if _DASHES[index] else ""
            parts.append(f'<polyline class="mesh{index} curve" data-mesh="{mesh}" '
                         f'points="{coordinate_text}"{dash}/>')
            for point, (x, y) in zip(points, coordinates):
                _marker(parts, index, x, y, 3.5, measurement="capacity",
                        title=f"{mesh} capacity · c{point.concurrency} · "
                              f"{_value(point.rps)} RPS · "
                              f"{_value(getattr(point, metric) / divisor)} {unit}")
        if not bulk:
            _text(parts, left, bottom + 66, "Measured controls · actual RPS → value", css="muted")
            # Controls are independent points: never put them on a capacity polyline,
            # align their x values to the target, or interpolate a matched-load value.
            for index, mesh in enumerate(_MESHES):
                point = controls[mesh]
                value = getattr(point, metric) / divisor
                x = left + width * x_rate(point.rps)
                y = bottom - height * y_axis(value)
                label = f"{mesh} · {_value(point.rps)} RPS → {_value(value)} {unit}"
                _marker(parts, index, x, y, 6, control=True, title=label,
                        measurement="shared")
                row_y = bottom + 88 + index * 20
                _marker(parts, index, left + 2, row_y - 5, 4, control=True)
                _text(parts, left + 19, row_y, label, size=13)
        else:
            _text(parts, left, bottom + 66, "Each marker is a measured capacity point.", css="muted")
            _text(parts, left, bottom + 88, "1 MiB request + 1 MiB response per exchange.", size=13)
            _text(parts, left, bottom + 110, "Payload rate is per direction, not doubled.", size=13)
        parts.append('</g>')

    _text(parts, 24, 951, "Only error-free points shown. Lines follow declared concurrency order; "
          "they do not add measurements.", css="muted", size=13)
    _text(parts, 24, 974, "Shading is the overlap of measured capacity RPS spans. "
          "Ringed controls use actual achieved RPS, not the offered target.", css="muted", size=13)
    parts.append('</svg>')
    return "\n".join(parts) + "\n"
