#!/usr/bin/env python3
"""Builds a Grafana dashboard for the glassos_* metrics into glassos.json next to itself.

Importing into Grafana: Dashboards → New → Import → upload glassos.json, choose
the Prometheus data source. The computer is chosen with the instance list at the top of the dashboard.
"""

import json
from pathlib import Path

DS = {"type": "prometheus", "uid": "${datasource}"}
SEL = 'instance=~"$instance"'

panels = []
next_id = 1
y = 0


def q(expr, legend="", instant=False):
    t = {"datasource": DS, "expr": expr.replace("SEL", SEL), "legendFormat": legend, "refId": chr(65 + len(_targets))}
    if instant:
        t["instant"] = True
        t["range"] = False
        t["format"] = "table"
    _targets.append(t)
    return t


_targets = []


def targets(*items):
    global _targets
    out = list(items)
    _targets = []
    return out


def panel(kind, title, x, w, h, tgts, unit=None, desc="", **extra):
    global next_id
    p = {
        "id": next_id,
        "type": kind,
        "title": title,
        "description": desc,
        "datasource": DS,
        "gridPos": {"x": x, "y": y, "w": w, "h": h},
        "targets": tgts,
        "fieldConfig": {"defaults": {}, "overrides": []},
        "options": {},
    }
    if unit:
        p["fieldConfig"]["defaults"]["unit"] = unit
    for key, value in extra.items():
        if key == "defaults":
            p["fieldConfig"]["defaults"].update(value)
        elif key == "overrides":
            p["fieldConfig"]["overrides"] = value
        else:
            p["options"].update(value) if key == "options" else p.update({key: value})
    next_id += 1
    panels.append(p)
    return p


def row(title):
    global y, next_id
    panels.append({"id": next_id, "type": "row", "title": title, "collapsed": False, "gridPos": {"x": 0, "y": y, "w": 24, "h": 1}, "panels": []})
    next_id += 1
    y += 1


def stat(title, x, w, expr, unit=None, thresholds=None, decimals=None, mappings=None, desc="", text=False):
    defaults = {"color": {"mode": "thresholds"}, "noValue": "\u2014"}
    if thresholds:
        defaults["thresholds"] = {"mode": "absolute", "steps": thresholds}
    else:
        defaults["thresholds"] = {"mode": "absolute", "steps": [{"color": "blue", "value": None}]}
    if decimals is not None:
        defaults["decimals"] = decimals
    if mappings:
        defaults["mappings"] = mappings
    options = {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False}, "colorMode": "value", "graphMode": "area", "textMode": "value" if not text else "name"}
    return panel("stat", title, x, w, 4, targets(q(expr, "{{instance}}" if not text else "")), unit, desc, defaults=defaults, options=options)


def series(title, x, w, h, items, unit=None, stack=False, desc="", fill=10, minimum=None, maximum=None, overrides=None):
    custom = {"fillOpacity": fill, "lineWidth": 1, "showPoints": "never", "spanNulls": True}
    if stack:
        custom["stacking"] = {"mode": "normal", "group": "A"}
        custom["fillOpacity"] = 60
    defaults = {"custom": custom}
    if minimum is not None:
        defaults["min"] = minimum
    if maximum is not None:
        defaults["max"] = maximum
    options = {"legend": {"displayMode": "list", "placement": "bottom", "showLegend": True}, "tooltip": {"mode": "multi", "sort": "desc"}}
    return panel("timeseries", title, x, w, h, targets(*[q(e, l) for e, l in items]), unit, desc, defaults=defaults, options=options, overrides=overrides or [])


def gauge(title, x, w, h, expr, legend, unit, maximum=1, thresholds=None, desc=""):
    defaults = {"min": 0, "max": maximum, "thresholds": {"mode": "absolute", "steps": thresholds or [{"color": "green", "value": None}, {"color": "orange", "value": 0.75 * maximum}, {"color": "red", "value": 0.9 * maximum}]}}
    options = {"reduceOptions": {"calcs": ["lastNotNull"], "fields": "", "values": False}, "displayMode": "gradient", "orientation": "horizontal", "showUnfilled": True}
    return panel("bargauge", title, x, w, h, targets(q(expr, legend)), unit, desc, defaults=defaults, options=options)


def table(title, x, w, h, items, desc="", overrides=None, organize=None):
    options = {"showHeader": True, "cellHeight": "sm"}
    p = panel("table", title, x, w, h, targets(*[q(e, l, instant=True) for e, l in items]), None, desc, options=options, overrides=overrides or [])
    p["transformations"] = organize or []
    return p


# ------------------------------------------------------------------ overview
row("Overview")
stat("Uptime", 0, 3, "glassos_uptime_seconds{SEL}", "s", decimals=0)
stat("CPU", 3, 3, "glassos_cpu_usage_ratio{SEL}", "percentunit",
     [{"color": "green", "value": None}, {"color": "orange", "value": 0.7}, {"color": "red", "value": 0.9}])
stat("Memory used", 6, 3, 'glassos_memory_bytes{SEL,state="used"} / ignoring(state) sum without(state) (glassos_memory_bytes{SEL})', "percentunit",
     [{"color": "green", "value": None}, {"color": "orange", "value": 0.75}, {"color": "red", "value": 0.9}])
stat("Load (1 min)", 9, 3, 'glassos_load{SEL,minutes="1"}', None,
     [{"color": "green", "value": None}, {"color": "orange", "value": 2}, {"color": "red", "value": 4}], decimals=2)
stat("Temperature", 12, 3, "glassos_cpu_temperature_celsius{SEL}", "celsius",
     [{"color": "green", "value": None}, {"color": "orange", "value": 75}, {"color": "red", "value": 90}])
stat("Power", 15, 3, 'glassos_power_watts{SEL,part="package"}', "watt", decimals=1)
stat("Battery", 18, 3, "glassos_battery_charge_ratio{SEL}", "percentunit",
     [{"color": "red", "value": None}, {"color": "orange", "value": 0.2}, {"color": "green", "value": 0.4}])
stat("Network", 21, 3, "glassos_network_up{SEL}", None,
     [{"color": "red", "value": None}, {"color": "green", "value": 1}],
     mappings=[{"type": "value", "options": {"0": {"text": "down"}, "1": {"text": "up"}}}])
y += 4
table("System", 0, 12, 5, [("glassos_info{SEL}", "")], desc="Name, version and processor of each computer.",
      organize=[{"id": "labelsToFields", "options": {}},
                {"id": "organize", "options": {"excludeByName": {"Time": True, "Value": True, "__name__": True, "job": True, "os": False}, "renameByName": {"hostname": "Computer", "cpu": "Processor", "version": "Version", "os": "System", "instance": "Address"}}}])
table("Signed in", 12, 6, 5, [("glassos_user_signed_in{SEL}", "")],
      organize=[{"id": "labelsToFields", "options": {}},
                {"id": "organize", "options": {"excludeByName": {"Time": True, "Value": True, "__name__": True, "job": True}, "renameByName": {"user": "User", "instance": "Address"}}}])
stat("Accounts", 18, 3, "glassos_users{SEL}", None)
stat("Shared folder clients", 21, 3, "glassos_file_sharing_clients{SEL}", None)
y += 1  # the tables are one row taller than the stats

# ------------------------------------------------------------------ CPU
y += 4
row("Processor")
series("CPU usage", 0, 12, 8, [("glassos_cpu_usage_ratio{SEL}", "all cores")], "percentunit", minimum=0, maximum=1)
series("Load", 12, 12, 8, [('glassos_load{SEL,minutes="1"}', "1 min"), ('glassos_load{SEL,minutes="5"}', "5 min"), ('glassos_load{SEL,minutes="15"}', "15 min")],
       desc="Threads ready to run, averaged. The main thread is not counted.", minimum=0)
y += 8
series("Cores busy", 0, 8, 8, [('rate(glassos_cpu_seconds_total{SEL,cpu="main",mode="busy"}[$__rate_interval])', "main core"),
                              ('rate(glassos_cpu_seconds_total{SEL,cpu="helpers"}[$__rate_interval]) / ignoring(cpu, mode) (glassos_cpus{SEL} - 1)', "helper cores, average")],
       "percentunit", minimum=0, maximum=1,
       desc="The main core runs the desktop and all threads. Helper cores only draw and copy in parallel.")
series("Switches and system calls", 8, 8, 8, [("rate(glassos_context_switches_total{SEL}[$__rate_interval])", "thread switches"),
                                              ("rate(glassos_syscalls_total{SEL}[$__rate_interval])", "system calls")], "ops")
series("Threads and processes", 16, 8, 8, [("glassos_threads{SEL}", "threads"), ("glassos_processes{SEL}", "processes")], minimum=0)
y += 8
series("Frequency", 0, 8, 7, [("glassos_cpu_frequency_hertz{SEL}", "core 0")], "hertz", minimum=0)
series("Temperature", 8, 8, 7, [("glassos_cpu_temperature_celsius{SEL}", "package")], "celsius")
series("Sleep states", 16, 8, 7, [("glassos_cpu_sleep_ratio{SEL}", "{{level}} {{state}}")], "percentunit", minimum=0, maximum=1,
       desc="Share of each second the processor spent in each sleep state. Deeper states save more power.")

# ------------------------------------------------------------------ power
y += 7
row("Power")
series("Power", 0, 12, 8, [("glassos_power_watts{SEL}", "{{part}}")], "watt", minimum=0)
series("Battery", 12, 12, 8, [("glassos_battery_charge_ratio{SEL}", "charge"), ("glassos_power_on_ac{SEL}", "charger"),
                              ("glassos_battery_charging{SEL}", "charging")], "percentunit", minimum=0, maximum=1)
y += 8
stat("Time left on battery", 0, 6, "glassos_battery_time_left_seconds{SEL}", "s", decimals=0)
stat("Lid", 6, 6, "glassos_lid_closed{SEL}", None, [{"color": "green", "value": None}],
     mappings=[{"type": "value", "options": {"0": {"text": "open"}, "1": {"text": "closed"}}}])
stat("Core 0 awake", 12, 6, "glassos_core0_busy_ratio{SEL}", "percentunit")
stat("Time off at last check", 18, 6, "glassos_time_sync_offset_seconds{SEL}", "s",
     desc="How far the clock was off at the last check against the time server.")

# ------------------------------------------------------------------ memory and disks
y += 4
row("Memory and disks")
series("Kernel memory", 0, 8, 8, [('glassos_memory_bytes{SEL}', "{{state}}")], "bytes", stack=True)
series("Disk throughput", 8, 8, 8, [('rate(glassos_disk_bytes_total{SEL,op="read"}[$__rate_interval])', "{{disk}} read"),
                                    ('-rate(glassos_disk_bytes_total{SEL,op="write"}[$__rate_interval])', "{{disk}} write")], "Bps",
       desc="Reads up, writes down.")
series("Disk busy", 16, 8, 8, [("rate(glassos_disk_busy_seconds_total{SEL}[$__rate_interval])", "{{disk}}")], "percentunit", minimum=0,
       desc="Share of time spent waiting for each kind of disk.")
y += 8
series("Disk operations", 0, 12, 7, [('rate(glassos_disk_operations_total{SEL}[$__rate_interval])', "{{disk}} {{op}}")], "iops")
gauge("Drives", 12, 12, 7, "glassos_filesystem_used_bytes{SEL} / glassos_filesystem_size_bytes{SEL}", "{{name}} {{path}}", "percentunit")

# ------------------------------------------------------------------ network
y += 7
row("Network")
series("Traffic", 0, 12, 8, [('rate(glassos_network_bytes_total{SEL,direction="receive"}[$__rate_interval])', "{{card}} in"),
                             ('-rate(glassos_network_bytes_total{SEL,direction="transmit"}[$__rate_interval])', "{{card}} out")], "Bps",
       desc="In up, out down.")
series("Packets and TCP trouble", 12, 12, 8, [('rate(glassos_network_packets_total{SEL}[$__rate_interval])', "{{direction}}"),
                                             ("rate(glassos_tcp_resent_total{SEL}[$__rate_interval])", "TCP resent"),
                                             ("rate(glassos_tcp_out_of_order_total{SEL}[$__rate_interval])", "TCP out of order")], "pps")
y += 8
series("Wi-Fi signal", 0, 8, 7, [("glassos_wifi_signal_dbm{SEL}", "better antenna")], "dBm")
series("Wi-Fi rate", 8, 8, 7, [("glassos_wifi_rate_bits_per_second{SEL}", "receive rate")], "bps", minimum=0)
series("Wi-Fi repeats", 16, 8, 7, [("glassos_wifi_retried_ratio{SEL}", "repeated frames")], "percentunit", minimum=0,
       desc="Share of frames the access point had to send again. High means a poor radio link.")

# ------------------------------------------------------------------ tasks and services
y += 7
row("Programs and services")
series("Busiest windows and threads", 0, 12, 9, [("topk(8, glassos_task_cpu_ratio{SEL})", "{{name}} ({{kind}})")], "percentunit", minimum=0,
       desc="Share of one core. Appears after someone signs in.")
series("Biggest windows and threads", 12, 12, 9, [("topk(8, glassos_task_memory_bytes{SEL})", "{{name}} ({{kind}})")], "bytes", minimum=0)
y += 9
table("Services", 0, 14, 7, [("glassos_service_up{SEL}", ""), ("increase(glassos_service_restarts_total{SEL}[1h])", "")],
      overrides=[
          {"matcher": {"id": "byName", "options": "Running"}, "properties": [
              {"id": "mappings", "value": [{"type": "value", "options": {"0": {"text": "stopped", "color": "red"}, "1": {"text": "running", "color": "green"}}}]},
              {"id": "custom.cellOptions", "value": {"type": "color-text"}}]},
          {"matcher": {"id": "byName", "options": "Restarts in 1 h"}, "properties": [{"id": "decimals", "value": 0}]}],
      organize=[{"id": "merge", "options": {}},
                {"id": "organize", "options": {"excludeByName": {"Time": True, "__name__": True, "job": True},
                                               "renameByName": {"name": "Service", "scope": "Kind", "user": "User", "instance": "Address",
                                                                "Value #A": "Running", "Value #B": "Restarts in 1 h"}}}])
series("Exporter", 14, 10, 7, [("rate(glassos_exporter_scrape_seconds_total{SEL}[$__rate_interval]) / rate(glassos_exporter_scrapes_total{SEL}[$__rate_interval])", "time per read")],
       "s", minimum=0, desc="How long building the metrics takes.")
y += 7

dashboard = {
    "title": "GlassOS",
    "uid": "glassos",
    "description": "Metrics of GlassOS computers (glassos_*).",
    "tags": ["glassos"],
    "timezone": "browser",
    "editable": True,
    "graphTooltip": 1,
    "refresh": "30s",
    "time": {"from": "now-3h", "to": "now"},
    "schemaVersion": 39,
    "version": 1,
    "templating": {"list": [
        {"name": "datasource", "label": "Prometheus", "type": "datasource", "query": "prometheus", "current": {}, "hide": 0},
        {"name": "instance", "label": "Computer", "type": "query", "datasource": DS,
         "query": {"query": "label_values(glassos_info, instance)", "refId": "instance"},
         "definition": "label_values(glassos_info, instance)", "refresh": 2, "includeAll": True, "multi": True, "sort": 1,
         "current": {"selected": True, "text": ["All"], "value": ["$__all"]}, "hide": 0},
    ]},
    "annotations": {"list": []},
    "panels": panels,
}

out = Path(__file__).with_name("glassos.json")
out.write_text(json.dumps(dashboard, indent=2, ensure_ascii=False) + "\n")
print(f"{out}: {len(panels)} panels")
