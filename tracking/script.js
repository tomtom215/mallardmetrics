(function(){'use strict';var d=document,w=window,l=d.currentScript,
u=l.getAttribute('data-api')||(new URL(l.src).origin+'/api/event'),
s=l.getAttribute('data-domain');
function t(n,o){if(d.visibilityState==='prerender')return;
o=o||{};
var b={d:s,n:n,u:o.url||w.location.href,r:d.referrer||null,
w:w.innerWidth};
if(o.props)b.p=JSON.stringify(o.props);
if(o.revenue!=null)b.ra=o.revenue;
if(o.currency)b.rc=o.currency;
var x=new XMLHttpRequest();x.open('POST',u,true);
x.setRequestHeader('Content-Type','application/json');
x.send(JSON.stringify(b));if(o.callback)x.onload=o.callback}
function p(){t('pageview')}
w.mallard=t;
var h=w.history;if(h.pushState){var o=h.pushState;
h.pushState=function(){o.apply(this,arguments);p()};
w.addEventListener('popstate',p)}
p()})();
