// Outline traced from the approved ghost on a 100 × 86 grid. Both arm poses
// join the same shoulder and flank so a wave only moves the raised arm.
const raisedArm = `C 82 35 85 32 83 27 C 81 20 87 15 94 17.5
  C 102 20.3 101 32 96.5 39.5 C 92.5 46 86.8 49.2 81 51`;
const loweredArm = `C 83 37 87 37 91 33 C 96 28 102 32 100 38
  C 98 45 90 51 81 51`;

function outline(arm) {
  return `M 18 43 C 17 34 15 24 24 12.5 C 30 4.5 40 0 49.8 0
    C 65.5 0 76.5 15 78.7 35.5 ${arm}
    C 81.5 57 86 70.7 81.8 76.5 C 78.8 81 74.2 75 69.5 78
    C 63 81.4 60.4 86.2 53.3 82.2 C 48.5 79.1 47.8 80.3 42.8 83.6
    C 37.2 87.4 32 87 28.6 82.5 C 25.1 78.3 20.1 82.7 14.7 83.4
    C 8.8 84.2 7.7 80 10.8 75.2 C 15.2 68.5 18.3 64 18.5 59
    C 12.7 60 3.4 62 0.7 55.8 C -2 49.5 3.9 45.4 11.1 44.8
    C 14.5 44.5 17.5 44.8 18 43 Z`;
}

// The little crescents give the original its sideways glance. In a template
// the eyes punch through to whatever color is behind the menu bar.
const eye = "M 37.4 28 C 38.3 20 45.9 17.3 49.1 23 C 52.8 29.8 51.4 37 47.4 36.5 C 43 36 40.8 28.5 44.8 22.6 Q 40.7 23.2 37.4 28 Z";

function eyes(expression = "open") {
  switch (expression) {
    case "blink": return '<path d="M39 28.5l10 -1.2 M59.5 25.1l10 -1.2" fill="none" stroke="currentColor" stroke-width="4.4" stroke-linecap="round"/>';
    case "sleep": return '<path d="M38.5 27q5.5 7 11.5 -1 M59 23.5q5.5 7 11.5 -1" fill="none" stroke="currentColor" stroke-width="4.4" stroke-linecap="round"/>';
    case "happy": return '<path d="M38.5 29q4.5 -10 11.5 -2 M59 25.5q4.5 -10 11.5 -2" fill="none" stroke="currentColor" stroke-width="4.8" stroke-linecap="round"/>';
    default: return `<path d="${eye}"/><path d="${eye}" transform="translate(20.4 -3.5)"/>`;
  }
}

function zee(x, y, size) {
  return `<path d="M${x} ${y}h${size}l${-size} ${size}h${size}" fill="none" stroke="currentColor" stroke-width="${size * 0.31}" stroke-linecap="round" stroke-linejoin="round"/>`;
}

function bang(y) {
  return `<g transform="translate(0 ${y})"><rect x="19.2" y="2.5" width="1.8" height="6.7" rx=".9"/><circle cx="20.1" cy="11.5" r="1"/></g>`;
}

function dots(n) {
  return [0, 1, 2].map((i) =>
    `<circle cx="${16.2 + i * 2.1}" cy="9" r=".78" opacity="${i < n ? 1 : 0.28}"/>`
  ).join("");
}

// Draw the body and arms into one mask, then punch out the expression. The
// native animator displays each 44 × 36 PNG in a 22 × 18 pt slot.
function template({ body, arms = "", cutouts, placement = "translate(0 0)", mark = "" }) {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="22" height="18" viewBox="0 0 22 18">
  <defs>
    <mask id="ghost" maskUnits="userSpaceOnUse" x="0" y="0" width="22" height="18">
      <g transform="${placement}">
        <path fill="white" d="${body}"/>${arms ? `\n        <path d="${arms}" fill="none" stroke="white" stroke-width="2.35" stroke-linecap="round"/>` : ""}
        <g fill="black" color="black">${cutouts}</g>
      </g>
    </mask>
  </defs>
  <rect width="22" height="18" fill="black" mask="url(#ghost)"/>${mark ? `\n  ${mark}` : ""}
</svg>\n`;
}

function wave(arm, expression) {
  return template({
    body: outline(arm),
    cutouts: eyes(expression),
    placement: "translate(2.2 1.5) scale(.175)",
  });
}

function idle(expression, glance = false) {
  return template({
    // Upright, with the tips of both arms hanging free of the hem.
    body: `M6.05 8.35 C5.95 4.8 7.5 2 10.55 1.9 C13.8 1.65 15.55 4.45 15.5 7.9
      C15.32 10.3 15.28 12.7 15.8 14.65 Q16.35 16.8 14.35 15.95
      Q12.9 15.05 12 16.05 Q10.8 17.5 9.55 16.1 Q8.7 15.1 7.4 15.95
      Q5.15 17.18 5.7 14.75 C6.25 12.55 6.05 10.05 6.05 8.35 Z`,
    arms: glance
      ? "M6.8 8.2 Q2.25 8.25 3.45 10.55 Q3.9 11.45 6.15 11.4 M14.8 8.2 Q16.55 9.45 17.1 11.95"
      : "M6.8 8.2 Q4.95 8.9 4 11.9 M14.8 8.2 Q16.65 8.9 17.6 11.7",
    cutouts: `<g transform="translate(${glance ? 2.65 : 2} 2.25) scale(.164)">${eyes(expression)}</g>`,
    placement: glance ? "rotate(3 11 15)" : undefined,
  });
}

function glide(arms, rise, tilt) {
  return template({
    // The scallops stream behind the head. The front hand reaches as the
    // trailing hand sweeps back, like two strokes of a swimmer's arms.
    body: `M2.25 7.65 C3.4 7.2 4.9 8.35 6.55 7.2 C8.45 5.85 9.45 3.8 12.4 3.15
      C15.45 2.4 17.9 4.3 18.05 7.1 C18.3 10.35 16.15 12.9 13.15 13.25
      C10.6 13.55 9.05 12.5 7.4 13.25 C5.75 13.95 4.6 15.3 3.65 14.6
      C2.8 13.9 4.8 12.4 3.45 12.35 C2.45 12.25 1.05 12.8 .95 11.85
      C.8 10.7 3.8 10.05 2.65 9.25 Q.95 8.25 2.25 7.65 Z`,
    arms,
    cutouts: `<g transform="translate(5.9 2.5) scale(.148)">${eyes()}</g>`,
    placement: `translate(0 ${rise}) rotate(${tilt} 11 9)`,
  });
}

function signal(arms, rise) {
  return template({
    body: `M4.12 8.45 C3.95 5.4 5.55 3.1 8.5 3 C11.45 2.85 13.25 5.25 13.2 8.3
      C13.1 10.7 13.45 12.9 14.05 14.7 Q14.9 17 12.65 16 Q11.25 15.1 10.2 16.15
      Q8.95 17.55 7.6 16.15 Q6.6 15.15 5.1 16.05 Q2.7 17.2 3.4 14.65
      C4.1 12.25 4.25 10.65 4.12 8.45 Z`,
    arms,
    cutouts: '<ellipse cx="7.15" cy="7.65" rx="1" ry="1.48"/><ellipse cx="10.45" cy="7.65" rx="1" ry="1.48"/>',
    placement: `translate(0 ${rise})`,
    mark: bang(rise),
  });
}

function nap(breath, mark) {
  return template({
    // Rest the head on the forearms and curl the trailing hem back up.
    body: `M2.2 12.35 C2 9.25 4.35 6.95 7.25 7.35 C10.55 7.7 10.9 10.3 13.25 10.1
      C14.8 10.05 15.25 9.1 15.85 8.52 C16.9 7.55 17.6 8.12 17.25 9.45
      C16.95 10.75 15.9 11.9 16.5 12.25 Q18 12.95 16.3 13.9
      C14.65 14.5 14.3 14.25 14.7 15 Q15 16.1 12 16 L5.55 16
      C3.6 16.2 2.35 15.45 2.2 12.35 Z`,
    arms: "M5.7 14.7 Q4.15 14.12 2.25 15.05 M9.4 14.9 Q7.7 14.35 6.15 15.22",
    cutouts: '<path d="M3.65 11.75q1 1.25 2.05 -.1 M6.95 11.35q1.02 1.25 2.1 -.12 M3.35 14.45q1.2 -.6 2.22 -.33" fill="none" stroke="currentColor" stroke-width=".8" stroke-linecap="round"/>',
    placement: `translate(0 16) scale(1 ${breath}) translate(0 -16)`,
    mark,
  });
}

function peek(lift, look, n) {
  return template({
    // Keep the paws and sill still while the head rises between them.
    body: `M3.9 12.95 C4 ${9.2 - lift} ${5.35 + look} ${6.2 - lift} ${8.65 + look} ${6.15 - lift}
      C${11.8 + look} ${6.1 - lift} 13.35 ${8.85 - lift} 13.55 12.95 Z`,
    arms: "M2.8 12.05q-.55 1.15 -.35 2.35 M14.45 12.05q.45 1.05 .3 2.15",
    cutouts: `<g transform="translate(${look} ${-lift})"><ellipse cx="7.05" cy="9.65" rx=".83" ry="1.25"/><ellipse cx="10.25" cy="9.5" rx=".83" ry="1.25"/></g>`,
    mark: '<path d="M1.35 13H16.25" stroke="black" stroke-width="1.05" stroke-linecap="round"/>' + dots(n),
  });
}

export const trayFrames = {
  "idle-0": idle("open"),
  "idle-1": idle("blink"),
  "idle-2": idle("open", true),
  "work-0": glide("M15.3 9q2.3 1.05 4.5 -.7 M10.5 11.7Q9.1 13.9 7.4 14.6", 0, 0),
  "work-1": glide("M15.3 9Q18.1 9.5 20 7 M10.5 11.7Q9.4 13.55 8.6 14.5", -.25, -3),
  "work-2": glide("M15.3 9Q17.7 11.75 19 11 M10.5 11.7Q8.7 13.05 6.2 13.8", .1, 2),
  "work-3": glide("M15.3 9Q17.5 10.9 19.4 9.9 M10.5 11.7Q8.9 13.45 6.7 14.15", .2, 1),
  "news-0": wave(raisedArm, "happy"),
  "news-1": wave(loweredArm, "happy"),
  "needs-0": signal("M5.1 9.6Q2.7 8.3 1.75 5.1 M12.3 9.6Q14.9 8.45 16.1 5.05", 0),
  "needs-1": signal("M5.1 9.6Q1.7 8.3 2.1 2.55 M12.3 9.6Q16.25 8.1 15.9 2.45", -.3),
  "off-0": nap(.94, zee(12.75, 5.2, 2.45)),
  "off-1": nap(1.05, zee(13.1, 4.65, 2.45) + zee(17.15, 2.3, 1.85)),
  "off-2": nap(1, zee(17.5, 1.75, 1.85)),
  "setup-0": peek(0, -.55, 1),
  "setup-1": peek(1.4, 0, 2),
  "setup-2": peek(2.4, .55, 3),
};

export const signatureTemplate = wave(raisedArm, "open");

// Empty states use the signature outline and its sleepy expression.
function colorMascot(arm, expression) {
  const sleepMark = expression === "sleep" ? `\n  <g color="#e76533">${zee(83, 8, 8)}${zee(96, -4, 6)}</g>` : "";
  return `<svg xmlns="http://www.w3.org/2000/svg" width="120" height="120" viewBox="-10 -17 120 120">
  <defs><linearGradient id="enamel" x2=".25" y2="1"><stop stop-color="#fff"/><stop offset="1" stop-color="#ffecdf"/></linearGradient></defs>
  <path d="${outline(arm)}" fill="url(#enamel)" stroke="#e8b39c" stroke-width="1.5" stroke-linejoin="round"/>
  <g fill="#39241b" color="#39241b">${eyes(expression)}</g>${sleepMark}
</svg>\n`;
}
export const uiMascots = {
  "guy.svg": colorMascot(raisedArm, "open"),
  "guy-sleepy.svg": colorMascot(loweredArm, "sleep"),
};
