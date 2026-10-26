// Champs Markdown (description, critères d'acceptation) : onglets
// Preview / Edit, aperçu rendu par le serveur (POST /markdown) et graphes
// mermaid dessinés dans le navigateur. mermaid.min.js (~5 Mo) n'est chargé
// que si la page contient un graphe.
(function () {
  'use strict';

  var mermaidLoad = null;

  function loadMermaid() {
    if (!mermaidLoad) {
      mermaidLoad = new Promise(function (resolve, reject) {
        var s = document.createElement('script');
        s.src = '/static/mermaid.min.js';
        s.onload = function () {
          window.mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: 'dark' });
          resolve(window.mermaid);
        };
        s.onerror = function () {
          mermaidLoad = null;
          reject(new Error('Could not load mermaid'));
        };
        document.head.appendChild(s);
      });
    }
    return mermaidLoad;
  }

  function renderMermaid(root) {
    var nodes = root.querySelectorAll('pre.mermaid:not([data-processed])');
    if (!nodes.length) return;
    loadMermaid()
      .then(function (m) { return m.run({ nodes: nodes, suppressErrors: true }); })
      .catch(function (e) { console.error(e); });
  }

  function show(field, tab) {
    var preview = field.querySelector('.md-preview');
    var area = field.querySelector('textarea');
    field.querySelectorAll('.md-tab').forEach(function (b) {
      b.classList.toggle('active', b.dataset.tab === tab);
    });
    if (tab === 'edit') {
      preview.hidden = true;
      area.hidden = false;
      area.focus();
      return;
    }
    area.hidden = true;
    preview.hidden = false;
    var text = area.value;
    if (field.dataset.rendered === text) return;
    fetch('/markdown', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({ text: text })
    })
      .then(function (r) { return r.ok ? r.text() : Promise.reject(new Error(r.statusText)); })
      .then(function (html) {
        preview.innerHTML = html;
        field.dataset.rendered = text;
        renderMermaid(preview);
      })
      .catch(function (e) { preview.textContent = 'Preview failed: ' + e.message; });
  }

  document.addEventListener('click', function (e) {
    var tab = e.target.closest('.md-tab');
    if (tab) show(tab.closest('.md-field'), tab.dataset.tab);
  });

  document.addEventListener('DOMContentLoaded', function () {
    // L'aperçu initial correspond au texte enregistré.
    document.querySelectorAll('.md-field').forEach(function (f) {
      f.dataset.rendered = f.querySelector('textarea').value;
    });
    renderMermaid(document);
  });
  document.addEventListener('htmx:afterSwap', function (e) { renderMermaid(e.target); });
})();

// Tableau : au survol d'une carte, mise en évidence de ses dépendances
// (tâches qu'elle attend, transitivement) et des tâches qui l'attendent ;
// le reste est estompé. Délégation sur le document : le tableau est
// remplacé toutes les 20 s par htmx.
(function () {
  'use strict';

  var CLASSES = ['dep-focus', 'dep-up', 'dep-down'];

  function depsOf(card) {
    return (card.dataset.deps || '').split(/[\s,]+/).filter(Boolean);
  }

  // Parcours transitif depuis `start` ; `next(id)` donne les voisins.
  function walk(start, next) {
    var seen = {}, stack = next(start), out = [];
    while (stack.length) {
      var id = stack.pop();
      if (seen[id] || id === start) continue;
      seen[id] = true;
      out.push(id);
      stack = stack.concat(next(id));
    }
    return out;
  }

  function clear(board) {
    board.classList.remove('dep-hover');
    board.querySelectorAll('.dep-focus, .dep-up, .dep-down').forEach(function (c) {
      c.classList.remove.apply(c.classList, CLASSES);
    });
  }

  function highlight(card) {
    var board = card.closest('.board');
    if (!board) return;
    var cards = {}, dependents = {};
    board.querySelectorAll('.card.task[data-id]').forEach(function (c) {
      cards[c.dataset.id] = c;
      depsOf(c).forEach(function (d) { (dependents[d] = dependents[d] || []).push(c.dataset.id); });
    });
    var id = card.dataset.id;
    var up = walk(id, function (x) { return cards[x] ? depsOf(cards[x]) : []; });
    var down = walk(id, function (x) { return dependents[x] || []; });
    clear(board);
    var found = false;
    up.forEach(function (x) { if (cards[x]) { cards[x].classList.add('dep-up'); found = true; } });
    down.forEach(function (x) { if (cards[x]) { cards[x].classList.add('dep-down'); found = true; } });
    if (found) {
      card.classList.add('dep-focus');
      board.classList.add('dep-hover');
    }
  }

  document.addEventListener('mouseover', function (e) {
    var card = e.target.closest && e.target.closest('.board .card.task');
    if (card && !card.classList.contains('dep-focus')) highlight(card);
  });
  document.addEventListener('mouseout', function (e) {
    var card = e.target.closest && e.target.closest('.board .card.task');
    if (!card || (e.relatedTarget && card.contains(e.relatedTarget))) return;
    var board = card.closest('.board');
    if (board) clear(board);
  });
  // Pas de surbrillance pendant un glisser-déposer.
  document.addEventListener('dragstart', function (e) {
    var board = e.target.closest && e.target.closest('.board');
    if (board) clear(board);
  });
})();
