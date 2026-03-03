/* Mallard Metrics — Mermaid diagram renderer
 * Loads mermaid.js from CDN and renders all code blocks tagged language-mermaid.
 * Applies the Pond Depth brand palette. */
(function () {
  'use strict';

  /* ---- Theme variables (Pond Depth palette) ---- */
  var LIGHT_VARS = {
    primaryColor:            '#E0F2F1',
    primaryTextColor:        '#14213D',
    primaryBorderColor:      '#0D7377',
    lineColor:               '#0D7377',
    secondaryColor:          '#FFF9E8',
    tertiaryColor:           '#EEF2F7',
    background:              '#FFFFFF',
    mainBkg:                 '#E0F2F1',
    nodeBorder:              '#0D7377',
    clusterBkg:              '#F0FAFB',
    clusterBorder:           '#0D7377',
    titleColor:              '#14213D',
    edgeLabelBackground:     '#FFFFFF',
    fontFamily:              'ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, sans-serif',
    fontSize:                '14px',
    labelBackground:         '#FFFFFF',
    attributeBackgroundColorEven: '#F0FAFB',
    attributeBackgroundColorOdd:  '#E0F2F1',
    fillType0:               '#E0F2F1',
    fillType1:               '#FFF9E8',
    fillType2:               '#EEF2F7',
    fillType3:               '#FFF0F0',
    fillType4:               '#F0FAFB',
    fillType5:               '#F8F9FA',
    fillType6:               '#E8F4F4',
    fillType7:               '#FFFBEB',
  };

  /* ---- Detect mdBook dark themes ---- */
  function isDarkTheme() {
    var cls = document.body ? document.body.className : '';
    return /\b(coal|navy|ayu)\b/.test(cls);
  }

  /* ---- Replace code blocks with mermaid divs and render ---- */
  function renderMermaid() {
    if (typeof globalThis.mermaid === 'undefined') return;

    globalThis.mermaid.initialize({
      startOnLoad: false,
      theme:        isDarkTheme() ? 'dark' : 'base',
      themeVariables: isDarkTheme() ? {} : LIGHT_VARS,
      flowchart: {
        curve:      'basis',
        htmlLabels: true,
        padding:    20,
      },
      sequence: {
        actorMargin:   60,
        messageMargin: 40,
      },
      er: { layoutDirection: 'TB' },
      securityLevel: 'loose',
    });

    /* Convert <pre><code class="language-mermaid">…</code></pre> → <div class="mermaid"> */
    var blocks = document.querySelectorAll('pre code.language-mermaid');
    [].forEach.call(blocks, function (code) {
      var pre = code.parentElement;
      var container = document.createElement('div');
      container.className = 'mermaid';
      container.textContent = code.textContent;
      if (pre && pre.parentNode) {
        pre.parentNode.replaceChild(container, pre);
      }
    });

    globalThis.mermaid.run({ querySelector: '.mermaid' });
  }

  /* ---- Load mermaid.js from CDN ---- */
  function loadMermaid() {
    var s = document.createElement('script');
    s.src  = 'https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js';
    s.async = true;
    s.onload = function () {
      if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', renderMermaid);
      } else {
        renderMermaid();
      }
    };
    document.head.appendChild(s);
  }

  /* ---- Entry point ---- */
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', loadMermaid);
  } else {
    loadMermaid();
  }
}());
