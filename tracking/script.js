(function(){'use strict';var d=document,w=window,l=d.currentScript,
u=l.getAttribute('data-api')||(new URL(l.src).origin+'/api/event'),
s=l.getAttribute('data-domain');
function t(n,p){if(d.visibilityState==='prerender')return;
var b={d:s,n:n,u:p||w.location.href,r:d.referrer||null,
w:w.innerWidth};
var x=new XMLHttpRequest();x.open('POST',u,true);
x.setRequestHeader('Content-Type','application/json');
x.send(JSON.stringify(b))}
function p(){t('pageview')}
var h=w.history;if(h.pushState){var o=h.pushState;
h.pushState=function(){o.apply(this,arguments);p()};
w.addEventListener('popstate',p)}
p()})();
