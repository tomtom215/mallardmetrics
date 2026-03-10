/* Mallard Metrics — Mermaid diagram renderer
 * Loads mermaid.js from CDN and renders all code blocks tagged language-mermaid.
 * Applies the Pond Depth brand palette for both light and dark themes. */
(function () {
  'use strict';

  /* ---- Theme variables — Light mode (Pond Depth palette) ---- */
  var LIGHT_VARS = {
    /* Node fills: medium teal so text is legible at normal contrast */
    primaryColor:                 '#14919B',
    primaryTextColor:             '#FFFFFF',
    primaryBorderColor:           '#0D7377',
    lineColor:                    '#2C3E50',
    secondaryColor:               '#FFF9E8',
    secondaryTextColor:           '#2C3E50',
    secondaryBorderColor:         '#C9971A',
    tertiaryColor:                '#E8EDF5',
    tertiaryTextColor:            '#2C3E50',
    tertiaryBorderColor:          '#7896AE',
    background:                   '#FFFFFF',
    mainBkg:                      '#14919B',
    nodeBorder:                   '#0D7377',
    clusterBkg:                   '#F0FAFB',
    clusterBorder:                '#0D7377',
    titleColor:                   '#14213D',
    edgeLabelBackground:          '#FFFFFF',
    fontFamily:                   'ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, sans-serif',
    fontSize:                     '14px',
    labelBackground:              '#FFFFFF',
    labelTextColor:               '#2C3E50',
    /* Actor fills in sequence diagrams */
    actorBkg:                     '#14919B',
    actorBorder:                  '#0D7377',
    actorTextColor:               '#FFFFFF',
    actorLineColor:               '#2C3E50',
    /* Activation / signal fills */
    activationBorderColor:        '#0D7377',
    activationBkgColor:           '#E0F2F1',
    /* Notes */
    noteBkgColor:                 '#FFF9E8',
    noteTextColor:                '#2C3E50',
    noteBorderColor:              '#C9971A',
    /* Loop fills */
    loopTextColor:                '#14213D',
    /* ER diagram */
    attributeBackgroundColorEven: '#F0FAFB',
    attributeBackgroundColorOdd:  '#E0F2F1',
    /* Fill types for subgraphs */
    fillType0: '#14919B',
    fillType1: '#F5C842',
    fillType2: '#E8EDF5',
    fillType3: '#FFF9E8',
    fillType4: '#F0FAFB',
    fillType5: '#E0F2F1',
    fillType6: '#D4EEF0',
    fillType7: '#FFFBEB',
  };

  /* ---- Theme variables — Dark mode ---- */
  var DARK_VARS = {
    /* Node fills: rich dark teal, legible on dark backgrounds */
    primaryColor:                 '#1a3d45',
    primaryTextColor:             '#c5cfe0',
    primaryBorderColor:           '#4EC9D4',
    lineColor:                    '#4EC9D4',
    secondaryColor:               '#2a2a14',
    secondaryTextColor:           '#c5cfe0',
    secondaryBorderColor:         '#C9971A',
    tertiaryColor:                '#1e2638',
    tertiaryTextColor:            '#c5cfe0',
    tertiaryBorderColor:          '#4a6080',
    background:                   '#181e2d',
    mainBkg:                      '#1a3d45',
    nodeBorder:                   '#4EC9D4',
    clusterBkg:                   '#141c28',
    clusterBorder:                '#2a3548',
    titleColor:                   '#c5cfe0',
    edgeLabelBackground:          '#1e2638',
    fontFamily:                   'ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, sans-serif',
    fontSize:                     '14px',
    labelBackground:              '#1e2638',
    labelTextColor:               '#c5cfe0',
    /* Actor fills in sequence diagrams */
    actorBkg:                     '#1a3d45',
    actorBorder:                  '#4EC9D4',
    actorTextColor:               '#c5cfe0',
    actorLineColor:               '#4EC9D4',
    /* Activation / signal fills */
    activationBorderColor:        '#4EC9D4',
    activationBkgColor:           '#12323a',
    /* Notes */
    noteBkgColor:                 '#2a2510',
    noteTextColor:                '#c5cfe0',
    noteBorderColor:              '#C9971A',
    /* Loop fills */
    loopTextColor:                '#c5cfe0',
    /* ER diagram */
    attributeBackgroundColorEven: '#141c28',
    attributeBackgroundColorOdd:  '#1a3d45',
    /* Fill types for subgraphs */
    fillType0: '#1a3d45',
    fillType1: '#2a250e',
    fillType2: '#1e2638',
    fillType3: '#2a2a14',
    fillType4: '#141c28',
    fillType5: '#12323a',
    fillType6: '#1a2a2a',
    fillType7: '#1e1e0e',
  };

  /* ---- Detect mdBook dark themes ---- */
  function isDarkTheme() {
    var cls = document.body ? document.body.className : '';
    return /\b(coal|navy|ayu)\b/.test(cls);
  }

  /* ---- Replace code blocks with mermaid divs and render ---- */
  function renderMermaid() {
    if (typeof globalThis.mermaid === 'undefined') return;

    var dark = isDarkTheme();
    globalThis.mermaid.initialize({
      startOnLoad:    false,
      theme:          'base',
      themeVariables: dark ? DARK_VARS : LIGHT_VARS,
      flowchart: {
        curve:      'basis',
        htmlLabels: true,
        padding:    24,
      },
      sequence: {
        actorMargin:   60,
        messageMargin: 40,
        mirrorActors:  false,
      },
      er:            { layoutDirection: 'TB' },
      securityLevel: 'strict',
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
    s.src   = 'https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js';
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
