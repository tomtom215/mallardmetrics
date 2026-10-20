// Applies the saved theme before first paint, so a dark-theme visitor on a light
// OS does not get a white flash. It must be a separate blocking script: the
// content security policy forbids an inline one.
try {
  var t = JSON.parse(localStorage.getItem('mallard.dashboard.v2')).theme;
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
} catch (e) { /* no saved preference */ }
