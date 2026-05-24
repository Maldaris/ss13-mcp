// Area-level rules: every station area should have an APC, air alarm, fire
// alarm, and at least one vent. Vents should be near the area centroid.
//
// Anchor "/area/station" matches all station areas (prefix subtype match).
// The "obj" passed to check() is an area descriptor:
//   {
//     path: "/area/station/...",
//     tiles: [[x,y,z], ...],
//     tile_count: N,
//     centroid: {x, y, z},
//     bbox: {x1, y1, x2, y2},
//     x, y, z   // for {x}/{y}/{z} message substitution (= centroid)
//   }

// Helper: scan an area's tiles for any object whose path starts with `prefix`.
// Returns the first match or null.
function findInArea(ctx, area, prefix) {
  for (var i = 0; i < area.tiles.length; i++) {
    var t = area.tiles[i];
    var here = ctx.at(t[0], t[1], t[2]);
    for (var j = 0; j < here.length; j++) {
      if (ctx.isType(here[j], prefix)) {
        return { x: t[0], y: t[1], z: t[2], obj: here[j] };
      }
    }
  }
  return null;
}

// Helper: collect all objects in an area matching a prefix.
function findAllInArea(ctx, area, prefix) {
  var results = [];
  for (var i = 0; i < area.tiles.length; i++) {
    var t = area.tiles[i];
    var here = ctx.at(t[0], t[1], t[2]);
    for (var j = 0; j < here.length; j++) {
      if (ctx.isType(here[j], prefix)) {
        results.push({ x: t[0], y: t[1], z: t[2], obj: here[j] });
      }
    }
  }
  return results;
}

// Maintenance areas: SS13 server convention varies on whether maint
// requires air/fire alarms. We exempt them from warning-level rules
// and surface them via info-level rules instead.
function isMaintenance(area) {
  return area.path.indexOf("/area/station/maintenance") === 0;
}

// Skip areas that don't need station fixtures. Mostly storage/sub-areas
// that share infrastructure with a parent area, plus external/intentionally
// under-built areas (solars, blast-test chambers, holodecks).
function isExempt(area) {
  var p = area.path;

  // Tiny areas (closets, sub-rooms) — fixtures live in parent area
  if (area.tile_count < 6) return true;

  // Hallways DO need fixtures
  if (p.indexOf("/area/station/hallway") === 0) return false;

  // Small maintenance sub-rooms share their parent's fixtures
  if (p.indexOf("/maintenance") !== -1 && area.tile_count < 12) return true;

  // External / intentionally-underbuilt areas. These either have no
  // atmosphere, no walls, or are designed as test chambers where fixtures
  // would be destroyed.
  var EXEMPT_PREFIXES = [
    "/area/station/solars",                          // external solar arrays
    "/area/station/science/ordnance/bomb",           // bomb test range
    "/area/station/science/ordnance/burnchamber",    // burn test chamber
    "/area/station/science/ordnance/freezerchamber", // freeze test chamber
    "/area/station/holodeck",                        // sim-generated terrain
    "/area/station/engineering/supermatter",         // SM chamber (intentionally bare)
  ];
  for (var i = 0; i < EXEMPT_PREFIXES.length; i++) {
    if (p.indexOf(EXEMPT_PREFIXES[i]) === 0) return true;
  }

  return false;
}

rule("area-needs-apc", {
  anchor: "/area/station",
  severity: "warning",
  message: "Area {area} has no APC (centroid ~({x},{y},{z}), {area_size} tiles)",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    if (findInArea(ctx, area, "/obj/machinery/power/apc")) return true;
    return "Area " + area.path + " has no APC (centroid ~(" +
      area.centroid.x + "," + area.centroid.y + "," + area.centroid.z +
      "), " + area.tile_count + " tiles)";
  }
});

rule("area-needs-air-alarm", {
  anchor: "/area/station",
  severity: "warning",
  message: "Area {area} has no air alarm",
  check: function(area, ctx) {
    if (isExempt(area) || isMaintenance(area)) return true;
    if (findInArea(ctx, area, "/obj/machinery/airalarm")) return true;
    return "Area " + area.path + " has no air alarm (centroid ~(" +
      area.centroid.x + "," + area.centroid.y + "), " + area.tile_count + " tiles)";
  }
});

rule("area-needs-fire-alarm", {
  anchor: "/area/station",
  severity: "warning",
  message: "Area {area} has no fire alarm",
  check: function(area, ctx) {
    if (isExempt(area) || isMaintenance(area)) return true;
    if (findInArea(ctx, area, "/obj/machinery/firealarm")) return true;
    return "Area " + area.path + " has no fire alarm (centroid ~(" +
      area.centroid.x + "," + area.centroid.y + "), " + area.tile_count + " tiles)";
  }
});

// Info-level variants: surface maintenance areas missing alarms without
// failing the build. Useful for spot-checks but not blocking.
rule("maintenance-missing-air-alarm", {
  anchor: "/area/station/maintenance",
  severity: "info",
  message: "Maintenance area {area} has no air alarm",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    if (findInArea(ctx, area, "/obj/machinery/airalarm")) return true;
    return "Maintenance area " + area.path + " has no air alarm (" +
      area.tile_count + " tiles)";
  }
});

rule("maintenance-missing-fire-alarm", {
  anchor: "/area/station/maintenance",
  severity: "info",
  message: "Maintenance area {area} has no fire alarm",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    if (findInArea(ctx, area, "/obj/machinery/firealarm")) return true;
    return "Maintenance area " + area.path + " has no fire alarm (" +
      area.tile_count + " tiles)";
  }
});

rule("area-needs-vent", {
  anchor: "/area/station",
  severity: "warning",
  message: "Area {area} has no atmospherics vent",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    // Both vent_pump (atmos in) and vent_scrubber (atmos out) count as
    // "atmospherics coverage" — but vents specifically deliver air.
    if (findInArea(ctx, area, "/obj/machinery/atmospherics/components/unary/vent_pump")) return true;
    return "Area " + area.path + " has no vent_pump (centroid ~(" +
      area.centroid.x + "," + area.centroid.y + "), " + area.tile_count + " tiles)";
  }
});

// Vents should be near the area centroid. We compute the minimum Chebyshev
// distance from any vent to the centroid, then compare to the area's
// "effective radius" (sqrt(tile_count / π)). A vent within radius/2 is good;
// further than radius is reported.
//
// Also reports the ratio of room size to vent count, to surface chronically
// under-vented rooms.
// Any area with a fire alarm should also have at least one firedoor.
// Fire alarms without firedoors are decorative — there's nothing for them to
// trigger. Conversely, areas with firedoors implicitly require a fire alarm
// (covered by area-needs-fire-alarm).
rule("fire-alarm-area-needs-firedoor", {
  anchor: "/area/station",
  severity: "warning",
  message: "Area {area} has a fire alarm but no firedoor",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    // Only fire when the area actually has a fire alarm
    if (!findInArea(ctx, area, "/obj/machinery/firealarm")) return true;
    if (findInArea(ctx, area, "/obj/machinery/door/firedoor")) return true;
    return "Area " + area.path + " has a fire alarm but no firedoor (" +
      area.tile_count + " tiles)";
  }
});

// Every airlock that crosses an area boundary should be paired with a firedoor
// on the same tile. Firedoors seal during atmospheric emergencies; an airlock
// at an area boundary without one creates an atmos leak vector.
//
// "Crosses an area boundary" = at least one N/S/E/W neighbor turf is in a
// different area than the tile the airlock is on. Airlocks fully inside one
// area (e.g. internal partitions) are skipped.
rule("airlock-boundary-needs-firedoor", {
  anchor: "/obj/machinery/door/airlock",
  severity: "warning",
  message: "Airlock at ({x},{y},{z}) crosses an area boundary but has no firedoor on its tile",
  check: function(obj, ctx) {
    var z = obj.z || 1;

    // Maintenance airlocks are governed by server convention — often bundled
    // with firedoors via mapping helpers, often deliberately left bare.
    // Skip them so this rule surfaces department-boundary issues only.
    if (ctx.isType(obj, "/obj/machinery/door/airlock/maintenance") ||
        ctx.isType(obj, "/obj/machinery/door/airlock/maintenance_hatch")) {
      return true;
    }

    var here = ctx.at(obj.x, obj.y, z);

    // Skip if this airlock already shares a tile with a firedoor
    for (var i = 0; i < here.length; i++) {
      if (ctx.isType(here[i], "/obj/machinery/door/firedoor")) return true;
    }

    // Check if airlock is on an area boundary
    var myArea = ctx.areaOf(obj.x, obj.y, z);
    if (!myArea) return true;

    // External / intentionally-underbuilt areas don't need firedoors.
    // We re-implement isExempt's prefix list inline (rules can't share state).
    var EXEMPT_PREFIXES = [
      "/area/station/solars",
      "/area/station/science/ordnance/bomb",
      "/area/station/science/ordnance/burnchamber",
      "/area/station/science/ordnance/freezerchamber",
      "/area/station/holodeck",
      "/area/station/engineering/supermatter",
      "/area/space",
      "/area/ocean",
    ];
    function exempt(p) {
      if (!p) return false;
      for (var k = 0; k < EXEMPT_PREFIXES.length; k++) {
        if (p.indexOf(EXEMPT_PREFIXES[k]) === 0) return true;
      }
      return false;
    }
    if (exempt(myArea)) return true;

    var dirs = [[0, 1], [0, -1], [1, 0], [-1, 0]];
    for (var d = 0; d < dirs.length; d++) {
      var nx = obj.x + dirs[d][0];
      var ny = obj.y + dirs[d][1];
      var nArea = ctx.areaOf(nx, ny, z);
      if (nArea && nArea !== myArea && !exempt(nArea)) {
        // Boundary detected — and we already know no firedoor is here
        return false;
      }
    }
    return true;
  }
});

rule("vent-coverage-and-centrality", {
  anchor: "/area/station",
  severity: "info",
  message: "Area {area} vent coverage: {detail}",
  check: function(area, ctx) {
    if (isExempt(area)) return true;
    var vents = findAllInArea(ctx, area, "/obj/machinery/atmospherics/components/unary/vent_pump");
    if (vents.length === 0) return true; // covered by area-needs-vent
    var cx = area.centroid.x, cy = area.centroid.y;
    var radius = Math.sqrt(area.tile_count / Math.PI);
    var bestDist = Infinity;
    var bestVent = null;
    for (var i = 0; i < vents.length; i++) {
      var dx = vents[i].x - cx;
      var dy = vents[i].y - cy;
      var dist = Math.max(Math.abs(dx), Math.abs(dy)); // Chebyshev
      if (dist < bestDist) {
        bestDist = dist;
        bestVent = vents[i];
      }
    }
    var tilesPerVent = area.tile_count / vents.length;

    // Report if closest vent is further than effective radius OR
    // tiles-per-vent ratio exceeds 50 (rough guideline).
    var issues = [];
    if (bestDist > radius) {
      issues.push("closest vent " + bestDist.toFixed(0) +
        " tiles from centroid (effective radius " + radius.toFixed(1) + ")");
    }
    if (tilesPerVent > 50 && area.tile_count > 30) {
      issues.push(vents.length + " vent(s) for " + area.tile_count +
        " tiles (" + tilesPerVent.toFixed(0) + " tiles/vent)");
    }
    if (issues.length === 0) return true;
    return "Area " + area.path + " vent coverage: " + issues.join("; ");
  }
});
