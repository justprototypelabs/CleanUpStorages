//! Shared, self-contained UI shell: one design-system stylesheet, an inline SVG icon set, a
//! sidebar/toolbar renderer, and shared client helpers. Every page in `web.rs` renders through
//! `shell()` so the six screens stay visually identical and no markup is duplicated.

pub const STYLE: &str = r##"
@font-face{font-family:'Inter';src:url(/assets/InterVariable.woff2) format('woff2');
 font-weight:100 900;font-style:normal;font-display:swap;}
@font-face{font-family:'JetBrains Mono';src:url(/assets/JetBrainsMono-Regular.woff2) format('woff2');font-weight:400;font-display:swap;}
@font-face{font-family:'JetBrains Mono';src:url(/assets/JetBrainsMono-Medium.woff2) format('woff2');font-weight:500;font-display:swap;}
@font-face{font-family:'Material Symbols Outlined';src:url(/assets/MaterialSymbolsOutlined.woff2) format('woff2');font-weight:100 700;font-style:normal;font-display:block;}
.material-symbols-outlined{font-family:'Material Symbols Outlined';font-weight:normal;font-style:normal;font-size:20px;
 line-height:1;letter-spacing:normal;text-transform:none;display:inline-block;white-space:nowrap;word-wrap:normal;
 direction:ltr;-webkit-font-feature-settings:'liga';-webkit-font-smoothing:antialiased;
 font-variation-settings:'FILL' 0,'wght' 300,'GRAD' 0,'opsz' 24;flex:none;}
:root{color-scheme:light dark;
 --font-ui:'Inter',-apple-system,"Segoe UI",Roboto,system-ui,sans-serif;
 --font-mono:'JetBrains Mono',ui-monospace,"SF Mono",Consolas,monospace;
 --bg:#fcf8fb;--sidebar-bg:#f6f3f5;--content:#ffffff;--elev:#ffffff;--fg:#1b1b1d;--mut:#5c626e;--mut2:#8a8f99;
 --line:#1b1b1d1a;--line-strong:#1b1b1d2b;--accent:#0071e3;--accent-text:#0059b5;--accent-weak:#0071e31a;
 --amber:#9a5b00;--amber-bg:#f59e0b26;--red:#ba1a1a;--red-bg:#ba1a1a17;--green:#1a7f37;--green-bg:#2ecc7122;--gray:#717785;
 --r-sm:6px;--r:8px;--r-lg:16px;
 --sh-sm:0 1px 3px #0000000d,0 1px 2px #0000001a;
 --sh-md:0 6px 16px #0000001f,0 1px 3px #00000014;--sh-lg:0 16px 40px #00000026;
 --sidebar:260px;--topbar:64px;}
:root[data-theme="light"]{
 --bg:#fcf8fb;--sidebar-bg:#f6f3f5;--content:#ffffff;--elev:#ffffff;--fg:#1b1b1d;--mut:#5c626e;--mut2:#8a8f99;
 --line:#1b1b1d1a;--line-strong:#1b1b1d2b;--accent:#0071e3;--accent-text:#0059b5;--accent-weak:#0071e31a;
 --amber:#9a5b00;--amber-bg:#f59e0b26;--red:#ba1a1a;--red-bg:#ba1a1a17;--green:#1a7f37;--green-bg:#2ecc7122;--gray:#717785;}
@media (prefers-color-scheme:dark){:root{
 --bg:#161618;--sidebar-bg:#1c1c1f;--content:#232327;--elev:#2e2e33;--fg:#f3f0f2;--mut:#9a9aa2;--mut2:#75757e;
 --line:#ffffff14;--line-strong:#ffffff26;--accent:#0a84ff;--accent-text:#57a8ff;--accent-weak:#0a84ff26;
 --amber:#ff9f0a;--amber-bg:#ff9f0a26;--red:#ff5a4d;--red-bg:#ff453a26;--green:#30d158;--green-bg:#30d15826;--gray:#98989d;}}
:root[data-theme="dark"]{
 --bg:#161618;--sidebar-bg:#1c1c1f;--content:#232327;--elev:#2e2e33;--fg:#f3f0f2;--mut:#9a9aa2;--mut2:#75757e;
 --line:#ffffff14;--line-strong:#ffffff26;--accent:#0a84ff;--accent-text:#57a8ff;--accent-weak:#0a84ff26;
 --amber:#ff9f0a;--amber-bg:#ff9f0a26;--red:#ff5a4d;--red-bg:#ff453a26;--green:#30d158;--green-bg:#30d15826;--gray:#98989d;}
*{box-sizing:border-box;}
body{margin:0;font:14px/1.45 var(--font-ui);letter-spacing:-.006em;
 background:var(--bg);color:var(--fg);-webkit-font-smoothing:antialiased;text-rendering:optimizeLegibility;}
.mono{font-family:var(--font-mono);font-variant-numeric:tabular-nums;font-size:12px;letter-spacing:-.02em;}
h1,h2,h3{letter-spacing:-.02em;font-weight:600;margin-top:0;}
h3{font-size:15px;} h2{font-size:22px;}
.page-h{font-size:27px;font-weight:700;letter-spacing:-.03em;margin:0 0 4px;}
.page-sub{color:var(--mut);font-size:14px;margin:0 0 22px;}
aside.side{position:fixed;left:0;top:0;bottom:0;width:var(--sidebar);display:flex;flex-direction:column;
 background:color-mix(in srgb,var(--sidebar-bg) 86%,transparent);
 backdrop-filter:blur(40px) saturate(180%);-webkit-backdrop-filter:blur(40px) saturate(180%);
 border-right:1px solid var(--line);padding:16px 0;overflow-y:auto;overflow-x:hidden;transition:width .18s ease;}
.side-head{display:flex;align-items:center;gap:8px;padding:2px 14px 20px;}
.side-head .brand{flex:1;min-width:0;overflow:hidden;}
aside.side h1{font-size:22px;margin:0;font-weight:700;letter-spacing:-.03em;line-height:1.1;white-space:nowrap;}
aside.side .tagline{margin:2px 0 0;font-size:12px;color:var(--mut2);white-space:nowrap;}
.rail-toggle{flex:none;width:32px;height:32px;border:0;background:transparent;color:var(--mut);border-radius:var(--r-sm);
 cursor:pointer;display:flex;align-items:center;justify-content:center;transition:background .12s;}
.rail-toggle:hover{background:var(--line);color:var(--fg);}
.rail-toggle .material-symbols-outlined{font-size:20px;transition:transform .18s;}
nav{padding:0 10px;display:flex;flex-direction:column;gap:2px;}
nav a{display:flex;gap:12px;align-items:center;padding:8px 12px;border-radius:var(--r);
 color:var(--mut);text-decoration:none;font-weight:500;font-size:13px;transition:background .12s,color .12s;white-space:nowrap;}
nav a .lb{overflow:hidden;}
nav a.active{background:var(--accent-weak);color:var(--accent-text);font-weight:600;}
nav a:hover:not(.active){background:var(--line);color:var(--fg);}
nav a .material-symbols-outlined{font-size:21px;}
/* collapsed icon rail */
:root[data-rail="1"]{--sidebar:70px;}
:root[data-rail="1"] .side-head{justify-content:center;padding:2px 0 20px;}
:root[data-rail="1"] .brand{display:none;}
:root[data-rail="1"] .rail-toggle .material-symbols-outlined{transform:rotate(180deg);}
:root[data-rail="1"] nav{padding:0 12px;}
:root[data-rail="1"] nav a{justify-content:center;padding:10px 0;}
:root[data-rail="1"] nav a .lb{display:none;}
:root[data-rail="1"] .themebar .lbl{display:none;}
:root[data-rail="1"] .themebar .seg{flex-direction:column;border-radius:var(--r-lg);}
:root[data-rail="1"] .themebar .seg button{justify-content:center;padding:7px 0;}
:root[data-rail="1"] .themebar .seg button .lb{display:none;}
main{margin-left:var(--sidebar);padding:36px 44px 40px;display:flex;flex-direction:column;
 min-height:100vh;min-height:100dvh;transition:margin-left .18s ease;}
main>*{max-width:none;margin:0;flex:none;}
main>.narrow{max-width:1120px;margin-left:auto;margin-right:auto;flex:1 1 auto;display:flex;flex-direction:column;min-height:0;width:100%;}
.narrow>.tree-card{flex:1 1 auto;min-height:0;overflow:auto;}
.card{background:var(--content);border:1px solid var(--line);border-radius:var(--r-lg);padding:22px;
 margin:0 0 20px;box-shadow:var(--sh-sm);}
.card.hover{transition:box-shadow .16s,transform .16s,border-color .16s;}
.card.hover:hover{box-shadow:var(--sh-md);transform:translateY(-1px);}
.grid{display:grid;grid-template-columns:repeat(12,1fr);gap:20px;}
.mut{color:var(--mut);} .row{display:flex;align-items:center;gap:8px;}
.btn{font:inherit;font-weight:500;font-size:13px;padding:8px 15px;border-radius:var(--r);border:1px solid var(--line-strong);
 background:var(--content);color:var(--fg);cursor:pointer;display:inline-flex;align-items:center;justify-content:center;gap:6px;
 transition:background .12s,box-shadow .12s,filter .12s;box-shadow:var(--sh-sm);white-space:nowrap;text-decoration:none;}
.btn:hover{background:var(--line);} .btn:active{transform:translateY(.5px);}
.btn .material-symbols-outlined{font-size:18px;}
.btn-primary{background:var(--accent);border-color:transparent;color:#fff;box-shadow:0 1px 2px #0071e340;}
.btn-primary:hover{filter:brightness(1.06);background:var(--accent);}
.btn-danger{background:var(--content);border-color:color-mix(in srgb,var(--red) 40%,var(--line-strong));color:var(--red);}
.btn-danger:hover{background:var(--red-bg);}
.card-ico{width:44px;height:44px;border-radius:10px;display:flex;align-items:center;justify-content:center;flex:none;
 background:var(--accent-weak);color:var(--accent);}
.card-ico .material-symbols-outlined{font-size:22px;}
.tag{font-size:11px;font-weight:500;padding:3px 9px;border-radius:6px;background:var(--line);color:var(--mut);font-family:var(--font-mono);}
.chiprow{display:flex;flex-wrap:wrap;gap:6px;margin:4px 0 12px;}
.chip{display:inline-flex;align-items:center;gap:5px;font-size:11px;font-family:var(--font-mono);
 background:var(--line);color:var(--mut);padding:3px 6px 3px 9px;border-radius:6px;}
.chip button{background:none;border:0;color:var(--mut);cursor:pointer;font-size:13px;line-height:1;padding:0 2px;font-family:inherit;}
.chip button:hover{color:var(--red);}
.linkbtn{display:inline-flex;align-items:center;gap:5px;background:none;border:0;cursor:pointer;color:var(--mut);font:inherit;font-size:12.5px;padding:2px 4px;border-radius:6px;}
.linkbtn:hover{color:var(--accent);}
.linkbtn .material-symbols-outlined{font-size:16px;}
.actrow{display:flex;align-items:center;gap:14px;padding:12px 2px;border-top:1px solid var(--line);}
.actrow:first-child{border-top:0;}
.act-ico{width:40px;height:40px;border-radius:10px;flex:none;display:flex;align-items:center;justify-content:center;}
.act-ico .material-symbols-outlined{font-size:20px;}
.act-title{font-weight:500;font-size:13.5px;}
.act-time{margin-left:auto;font-size:12px;color:var(--mut2);font-family:var(--font-mono);white-space:nowrap;}
.tone-red{background:var(--red-bg);color:var(--red);} .tone-blue{background:var(--accent-weak);color:var(--accent);}
.tone-gray{background:var(--line);color:var(--mut);} .tone-green{background:var(--green-bg);color:var(--green);}
.tone-amber{background:var(--amber-bg);color:var(--amber);}
.icon-btn{border:0;background:transparent;color:var(--mut);border-radius:999px;width:30px;height:30px;
 display:inline-flex;align-items:center;justify-content:center;cursor:pointer;box-shadow:none;padding:0;}
.icon-btn:hover{background:var(--line);color:var(--fg);}
.pill{font-size:11px;padding:2px 9px;border-radius:999px;font-weight:600;letter-spacing:.01em;
 display:inline-flex;align-items:center;gap:5px;}
.pill::before{content:"";width:6px;height:6px;border-radius:50%;background:currentColor;}
.pill.quarantined{color:var(--amber);background:var(--amber-bg);}
.pill.missing{color:var(--red);background:var(--red-bg);}
.pill.active{color:var(--green);background:var(--green-bg);}
.pill.purged,.pill.offline{color:var(--gray);background:var(--line);}
table{width:100%;border-collapse:collapse;}
th,td{text-align:left;padding:9px 10px;border-bottom:1px solid var(--line);vertical-align:top;}
th{color:var(--mut);font-weight:600;font-size:11px;text-transform:uppercase;letter-spacing:.05em;}
.progressbar{height:8px;background:var(--line);border-radius:999px;overflow:hidden;}
.progressbar>span{display:block;height:100%;border-radius:999px;
 background:linear-gradient(90deg,var(--accent),color-mix(in srgb,var(--accent) 70%,#fff));}
.console-out{font-family:var(--font-mono),ui-monospace,Consolas,monospace;white-space:pre-wrap;background:var(--content);
 border:1px solid var(--line);border-radius:var(--r-lg);padding:18px;flex:1 1 auto;min-height:260px;overflow:auto;
 font-size:12.5px;box-shadow:var(--sh-sm);}
.console-inbar{display:flex;align-items:center;gap:10px;margin-top:12px;background:var(--content);border:1px solid var(--line-strong);
 border-radius:var(--r);padding:0 16px;box-shadow:var(--sh-sm);}
.console-inbar:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
.console-inbar .prompt{color:var(--green);font-family:var(--font-mono);font-weight:600;flex:none;}
.console-in{flex:1;font-family:var(--font-mono),ui-monospace,Consolas,monospace;padding:13px 0;border:0;
 background:transparent;color:var(--fg);font-size:12.5px;}
.console-in:focus{outline:none;box-shadow:none;}
.cards{display:flex;flex-wrap:wrap;gap:16px;}
.cards .card{width:250px;margin:0;}
.rv-page{flex:1 1 auto;min-height:0;display:flex;flex-direction:column;}
#group{flex:1 1 auto;min-height:0;display:flex;}
#group .cards{flex:1 1 auto;min-height:0;justify-content:center;align-items:stretch;gap:20px;width:100%;}
#group .card{width:auto;flex:1 1 0;min-width:260px;max-width:460px;}
#group .empty{margin:auto;text-align:center;}
.drive-ico{width:38px;height:38px;border-radius:9px;flex:none;display:flex;align-items:center;justify-content:center;
 background:color-mix(in srgb,var(--dc) 16%,transparent);color:var(--dc);}
.drive-ico svg{width:20px;height:20px;}
.card.rvcard{padding:0;overflow:hidden;display:flex;flex-direction:column;}
.rvthumb{position:relative;background:var(--line);flex:1 1 auto;min-height:150px;}
.rvthumb img,.rvthumb .noimg{width:100%;height:100%;object-fit:cover;border-radius:0;display:block;margin:0;}
/* The rule above forces display:block, which kills .noimg's centering and drops its text into the
   top-left corner, under the "Keep this" pill. Restore the centering (must come after). */
.rvthumb .noimg{display:flex;align-items:center;justify-content:center;}
.rvbody{padding:15px 16px 16px;flex:none;}
.rvpath{font-family:var(--font-mono);font-size:12.5px;color:var(--fg);word-break:break-all;margin:0 0 12px;line-height:1.45;}
.rvcard.keep .rvpath{color:var(--accent-text);}
.dl{display:flex;justify-content:space-between;gap:12px;font-size:13px;padding:6px 0;border-top:1px solid var(--line);}
.dl:first-of-type{border-top:0;}
.dl .k{color:var(--mut);} .dl .v{font-weight:500;text-align:right;}
.keep-pill{position:absolute;top:10px;left:10px;background:var(--accent);color:#fff;font-size:10px;font-weight:700;
 letter-spacing:.04em;text-transform:uppercase;padding:5px 10px;border-radius:999px;display:inline-flex;align-items:center;gap:4px;
 box-shadow:0 1px 4px #00000030;}
.keep-pill .material-symbols-outlined{font-size:13px;}
.cards .card.keep{border-color:var(--accent);box-shadow:0 0 0 2px var(--accent) inset,var(--sh-md);}
.rvbar{display:flex;align-items:center;justify-content:space-between;gap:12px;margin-top:26px;}
.thumb{width:100%;height:150px;object-fit:contain;border-radius:var(--r-sm);background:var(--line);display:block;}
.noimg{width:100%;height:150px;display:flex;align-items:center;justify-content:center;color:var(--mut);
 background:var(--line);border-radius:var(--r-sm);font-size:12px;text-align:center;padding:8px;}
.badge{font-size:11px;color:var(--accent);font-weight:600;margin-top:8px;display:block;}
.arch{color:var(--mut);font-size:11px;margin-top:8px;}
.branch{margin-left:14px;border-left:1px solid var(--line);padding-left:8px;}
details.drive>summary,details.folder>summary{cursor:pointer;padding:7px 9px;border-radius:var(--r-sm);
 list-style:none;display:flex;align-items:center;gap:8px;user-select:none;font-size:13.5px;}
details>summary::-webkit-details-marker{display:none;}
details>summary::before{content:"\25B8";color:var(--mut);font-size:11px;width:10px;transition:transform .12s;flex:none;}
details[open]>summary::before{transform:rotate(90deg);}
details.drive>summary:hover,details.folder>summary:hover{background:var(--line);}
.dot{width:10px;height:10px;border-radius:50%;flex:none;box-shadow:inset 0 0 0 1px #00000018;}
.fico{font-size:18px!important;color:var(--mut);flex:none;}
.searchbar{display:flex;align-items:center;gap:11px;background:var(--content);border:1px solid var(--line-strong);
 border-radius:var(--r);padding:11px 16px;box-shadow:var(--sh-sm);margin-bottom:14px;transition:border-color .12s,box-shadow .12s;}
.searchbar:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
.searchbar .material-symbols-outlined{color:var(--mut);font-size:20px;}
.searchbar input{flex:1;font:inherit;font-size:14px;color:var(--fg);}
.browsetools{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-bottom:14px;}
.browsetools .count{margin-left:auto;font-size:13px;}
/* `.browsetools` sets display:flex, which outranks the UA sheet's [hidden]{display:none} at equal
   specificity — without this the collapsed row is never actually collapsed. */
.browsetools[hidden]{display:none;}
#rangerow{margin-top:-6px;}
.rangelbl{display:inline-flex;align-items:center;gap:7px;font-size:12.5px;color:var(--mut);}
.rangelbl input[type=date]{font:inherit;font-size:12.5px;color:var(--fg);background:var(--content);
 border:1px solid var(--line-strong);border-radius:8px;padding:5px 8px;}
.rangelbl input[type=date]:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
/* Mirrors .dd.multi.has-sel: a filter that is doing something must look different from one that isn't. */
.linkbtn.has-sel{color:var(--accent-text);background:var(--accent-weak);}
#copies[hidden]{display:none;}
.copyhead{font-size:13px;margin-bottom:8px;}
.copyrow{display:flex;gap:10px;align-items:baseline;justify-content:space-between;
 font-size:12.5px;padding:4px 0;border-top:1px solid var(--line);}
.copyrow .mono{word-break:break-all;}
.dd{position:relative;display:inline-block;}
.dd-btn{display:inline-flex;align-items:center;gap:6px;background:var(--content);border:1px solid var(--line-strong);
 border-radius:999px;padding:8px 10px 8px 15px;font:inherit;font-size:13px;font-weight:500;color:var(--fg);
 cursor:pointer;box-shadow:var(--sh-sm);transition:background .12s,border-color .12s,box-shadow .12s;}
.dd-btn:hover{background:var(--line);}
.dd.open .dd-btn{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
.dd-caret{font-size:18px;color:var(--mut);transition:transform .16s;margin-left:-1px;}
.dd.open .dd-caret{transform:rotate(180deg);}
.dd-menu{position:absolute;top:calc(100% + 6px);left:0;min-width:190px;z-index:30;background:var(--elev);
 border:1px solid var(--line-strong);border-radius:var(--r);box-shadow:var(--sh-lg);padding:6px;
 display:flex;flex-direction:column;gap:1px;max-height:min(60vh,340px);overflow:auto;
 transform-origin:top left;animation:ddin .13s ease;}
.dd-menu[hidden]{display:none;}
@keyframes ddin{from{opacity:0;transform:translateY(-4px) scale(.98);}to{opacity:1;transform:none;}}
.dd-opt{display:flex;align-items:center;gap:10px;text-align:left;white-space:nowrap;width:100%;
 background:none;border:0;border-radius:var(--r-sm);padding:7px 10px;font:inherit;font-size:13px;color:var(--fg);cursor:pointer;}
.dd-opt .dd-t{flex:1;}
.dd-opt:hover{background:var(--line);}
.dd-box{width:17px;height:17px;border:1.5px solid var(--line-strong);border-radius:5px;flex:none;
 display:flex;align-items:center;justify-content:center;transition:background .1s,border-color .1s;}
.dd-opt.sel .dd-box{background:var(--accent);border-color:var(--accent);}
.dd-ck{font-size:13px!important;color:#fff;visibility:hidden;}
.dd-opt.sel .dd-ck{visibility:visible;}
.dd-opt.sel{color:var(--accent-text);font-weight:600;}
.dd-badge{font-size:11px;font-weight:600;color:var(--mut);font-family:var(--font-mono);min-width:12px;text-align:right;}
.dd-opt.sel .dd-badge{color:var(--accent-text);}
.dd-opt.zero .dd-t{color:var(--mut2);}
.dd-opt.zero .dd-badge{opacity:.55;}
.dd-clear{margin-top:4px;border:0;border-top:1px solid var(--line);border-radius:0;background:none;width:100%;
 text-align:left;padding:8px 10px 4px;font:inherit;font-size:12px;color:var(--mut);cursor:pointer;}
.dd-clear:hover{color:var(--accent);}
.dd.multi.has-sel .dd-btn{border-color:var(--accent);color:var(--accent-text);background:var(--accent-weak);}
.dd.multi.has-sel .dd-caret{color:var(--accent-text);}
.leaf{display:flex;align-items:center;gap:9px;padding:6px 10px 6px 22px;border-radius:var(--r-sm);cursor:default;}
.leaf:hover{background:var(--line);}
.leaf .fname{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.leaf .meta{font-size:12px;white-space:nowrap;}
.leaf.dup{background:color-mix(in srgb,var(--dup) 9%,transparent);cursor:pointer;}
.leaf.hl{background:color-mix(in srgb,var(--dup) 24%,transparent);box-shadow:inset 0 0 0 1px var(--dup);}
.diamond{font-size:11px;font-weight:700;color:var(--dup);flex:none;}
.stat{font-size:23px;font-weight:600;letter-spacing:-.02em;}
.hero{position:relative;overflow:hidden;padding:30px 32px;}
.hero-glow{position:absolute;top:-60%;right:-10%;width:60%;height:220%;pointer-events:none;
 background:radial-gradient(closest-side,var(--accent-weak),transparent 72%);opacity:.9;}
.hero-label{position:relative;font-size:11px;font-weight:600;letter-spacing:.09em;text-transform:uppercase;color:var(--accent-text);}
.hero-stat{position:relative;font-size:40px;font-weight:700;letter-spacing:-.03em;margin:8px 0 4px;line-height:1.05;}
.tiles{display:flex;gap:12px;flex-wrap:wrap;}
.tile{flex:1;min-width:120px;background:var(--content);border:1px solid var(--line);border-radius:var(--r);padding:12px 14px;box-shadow:var(--sh-sm);}
.tile .k{font-size:11px;color:var(--mut);text-transform:uppercase;letter-spacing:.05em;}
.tile .v{font-size:22px;font-weight:650;margin-top:2px;}
.sec-label{font-size:11px;font-weight:600;letter-spacing:.08em;text-transform:uppercase;color:var(--mut);margin:26px 0 12px;}
.sec-label:first-child{margin-top:4px;}
.drivegrid{display:grid;grid-template-columns:1fr 1fr;gap:16px;}
/* Drives page only: one card per row. Half-width cards left the completeness table too narrow to
   read a path in. The Scan page's detected-drives grid keeps two columns -- those cards are small. */
.drivegrid.onecol{grid-template-columns:1fr;}
.dcard{cursor:pointer;transition:border-color .12s,box-shadow .12s;}
.dcard:hover{border-color:var(--line-strong);}
.dcard.sel{border-color:var(--accent);box-shadow:0 0 0 2px var(--accent) inset;}
.dcard .dtop{display:flex;gap:14px;align-items:flex-start;}
.dcard .dtop .txt{flex:1;min-width:0;}
.dcard .dname{font-weight:600;font-size:14.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;}
.dcard .dpath{font-family:var(--font-mono);font-size:11.5px;color:var(--mut);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;margin-top:2px;}
.dcard .dcap{display:flex;justify-content:space-between;font-size:11.5px;color:var(--mut);margin-top:7px;}
.statcols{display:grid;grid-template-columns:repeat(4,1fr);gap:8px;margin-top:18px;}
.statcol{border-left:1px solid var(--line);padding-left:16px;}
.statcol:first-child{border-left:0;padding-left:0;}
.statcol .k{font-size:11px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--mut);}
.statcol .v{font-size:26px;font-weight:700;letter-spacing:-.02em;margin-top:5px;}
.statcol .v.accent{color:var(--accent-text);}
.recentrow{display:flex;align-items:center;gap:12px;padding:13px 0;border-top:1px solid var(--line);}
.recentrow:first-child{border-top:0;}
.folderbar{display:flex;align-items:center;gap:14px;flex-wrap:wrap;}
.folderin{flex:1;min-width:180px;display:flex;align-items:center;gap:10px;background:var(--content);border:1px solid var(--line-strong);border-radius:var(--r);padding:11px 14px;box-shadow:var(--sh-sm);transition:border-color .12s,box-shadow .12s;}
.folderin:focus-within{border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
.folderin input{flex:1;font:inherit;color:var(--fg);}
.drivecard{margin:0;display:flex;flex-direction:column;padding:20px;}
.drivecard.err{border-color:color-mix(in srgb,var(--red) 30%,var(--line));background:color-mix(in srgb,var(--red-bg) 60%,var(--content));}
.drivecard .dhead{display:flex;gap:14px;align-items:flex-start;margin-bottom:16px;}
.drivecard .lastscan{margin-left:auto;font-size:12px;color:var(--mut);white-space:nowrap;padding-top:2px;}
.status-line{display:flex;align-items:center;gap:7px;font-size:12.5px;color:var(--mut);margin-top:4px;}
.status-line .sdot{width:8px;height:8px;border-radius:50%;flex:none;}
.cap-line{display:flex;justify-content:space-between;font-size:13px;margin:0 0 8px;}
.cap-line .pct{color:var(--mut);}
.drivecard .actions{display:flex;gap:10px;margin-top:16px;padding-top:14px;border-top:1px solid var(--line);}
.drivecard .actions .btn{flex:1;}
.drivecard .actions .iconbtn{flex:0 0 auto;width:40px;padding:8px 0;}
.drivecard .actions .iconbtn .material-symbols-outlined{font-size:19px;}
.q-alert{display:flex;align-items:center;gap:14px;background:color-mix(in srgb,var(--red-bg) 70%,var(--content));
 border:1px solid color-mix(in srgb,var(--red) 22%,var(--line));border-radius:var(--r-lg);padding:12px 14px;}
.q-alert .q-ico{width:40px;height:40px;border-radius:50%;flex:none;display:flex;align-items:center;justify-content:center;background:var(--red-bg);color:var(--red);}
.q-alert .qtxt strong{color:var(--red);display:block;font-size:13.5px;}
.q-alert .qtxt span{font-size:12px;color:var(--mut);}
.completeness{margin-top:14px;padding-top:14px;border-top:1px solid var(--line);font-size:12.5px;}
.completeness summary{cursor:pointer;color:var(--mut);user-select:none;}
/* Grid on the CONTAINER, with rows as display:contents, so every row's cells share the same
   three tracks. A per-row flexbox sizes each row independently, which is why nothing lined up. */
.completeness .cbody{margin-top:8px;max-height:260px;overflow-y:auto;overflow-x:hidden;
  display:grid;grid-template-columns:max-content minmax(0,1.6fr) minmax(0,1fr);gap:3px 12px;align-items:baseline;}
.erow{display:contents;}
.cnote{grid-column:1/-1;color:var(--mut);}
/* One line per row by default: an unbounded wrap turns a long path into a paragraph and destroys
   the alignment the grid just bought. Full text is in title=, and clicking the row reveals it. */
.epath,.ereason{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0;}
.epath{font-family:var(--font-mono);}
.epath b{font-weight:600;}
.ereason{color:var(--mut);}
.erow.open .epath,.erow.open .ereason{white-space:normal;word-break:break-all;}
.completeness .cbody .erow>span{cursor:pointer;padding:1px 0;}
.sumgrid{display:grid;grid-template-columns:repeat(3,1fr);gap:16px;}
.sumcard{margin:0;}
.sumcard .k{font-size:11px;font-weight:600;letter-spacing:.06em;text-transform:uppercase;color:var(--mut);}
.sumcard .v{font-size:31px;font-weight:700;letter-spacing:-.02em;margin:8px 0 6px;}
.sumcard .v.accent{color:var(--accent-text);}
.sumcard .s{font-size:12.5px;color:var(--mut);}
.switch{position:relative;display:inline-block;width:38px;height:22px;}
.switch input{opacity:0;width:0;height:0;}
.switch .sl{position:absolute;inset:0;background:var(--line-strong);border-radius:999px;transition:.15s;}
.switch .sl::before{content:"";position:absolute;width:16px;height:16px;left:3px;top:3px;background:#fff;border-radius:50%;transition:.15s;box-shadow:var(--sh-sm);}
.switch input:checked + .sl{background:var(--accent);}
.switch input:checked + .sl::before{transform:translateX(16px);}
#pbar.run>span{animation:indet 1.2s infinite ease-in-out;}
@keyframes indet{0%{margin-left:-40%}100%{margin-left:100%}}
select,input[type=text],input[type=search],textarea{background:var(--content);color:var(--fg);
 border:1px solid var(--line-strong);border-radius:var(--r-sm);padding:8px 10px;font:inherit;color-scheme:inherit;}
select:focus,input:focus,textarea:focus{outline:none;border-color:var(--accent);box-shadow:0 0 0 3px var(--accent-weak);}
option{background:var(--content);color:var(--fg);}
/* inputs that live inside a styled field container carry no border of their own (no double box) */
.searchbar input,.folderin input,.console-in{border:0;background:transparent;padding:0;border-radius:0;box-shadow:none;}
.searchbar input:focus,.folderin input:focus,.console-in:focus{outline:none;box-shadow:none;border:0;}
.seg{display:inline-flex;background:var(--line);border-radius:999px;padding:2px;gap:2px;}
.seg button{border:0;background:transparent;color:var(--mut);border-radius:999px;padding:4px 9px;
 cursor:pointer;display:flex;align-items:center;gap:5px;font:inherit;font-size:12px;}
.seg button.on{background:var(--content);color:var(--fg);box-shadow:var(--sh-sm);}
.seg button .material-symbols-outlined{font-size:15px;}
.themebar{margin-top:auto;padding-top:14px;border-top:1px solid var(--line);}
.themebar .lbl{font-size:11px;color:var(--mut);text-transform:uppercase;letter-spacing:.05em;display:block;margin:0 4px 6px;}
@media (max-width:1100px){
  .grid>*{grid-column:1 / -1 !important;}
  .drivegrid{grid-template-columns:1fr;}
  .sumgrid{grid-template-columns:1fr;}
  main{padding-left:24px;padding-right:24px;}
}
@media (max-width:820px){
  :root{--sidebar:200px;}
  aside.side h1{font-size:19px;}
  .statcols{grid-template-columns:1fr 1fr;}
  #group .cards{flex-direction:column;align-items:stretch;}
  #group .card{width:auto;}
  .folderbar{gap:10px;}
}
@media (max-width:640px){
  .statcols{grid-template-columns:1fr 1fr;}
  .cards .card{width:100%;}
}
"##;

pub const SHARED_JS: &str = r##"
const $=s=>document.querySelector(s);
const CSRF=(document.querySelector('meta[name="csrf"]')||{}).content||"";
function esc(s){return (s==null?"":String(s)).replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));}
function fmtSize(n){if(n==null)return"—";const u=["B","KB","MB","GB","TB"];let i=0,x=Number(n);while(x>=1024&&i<u.length-1){x/=1024;i++;}return (i?x.toFixed(1):x)+" "+u[i];}
function fmtDate(t){return t?new Date(t*1000).toISOString().slice(0,10):"—";}
function fmtAgo(t){if(!t)return"—";const s=Math.max(0,Math.floor(Date.now()/1000-t));if(s<60)return s+"s ago";const m=Math.floor(s/60);if(m<60)return m+"m ago";const h=Math.floor(m/60);if(h<24)return h+"h ago";const d=Math.floor(h/24);if(d<7)return d+"d ago";return fmtDate(t);}
function hueOf(s){let h=0;for(let i=0;i<String(s).length;i++)h=(h*31+String(s).charCodeAt(i))>>>0;return h%360;}
function driveColor(id){return `hsl(${hueOf(id)},46%,52%)`;}
const fmtN=n=>Number(n).toLocaleString();
function dupColor(hash){return `hsl(${hueOf(hash)},42%,56%)`;}
async function apiGet(u){const r=await fetch(u);if(!r.ok)throw new Error(await r.text());return r.json();}
async function apiPost(u,body){const r=await fetch(u,{method:"POST",headers:{"content-type":"application/json","x-cleanup-token":CSRF},body:JSON.stringify(body||{})});if(!r.ok)throw new Error(await r.text());return r.json();}
function applyTheme(t){ if(t==='auto'){localStorage.removeItem('theme');delete document.documentElement.dataset.theme;}
  else{localStorage.setItem('theme',t);document.documentElement.dataset.theme=t;}
  for(const b of document.querySelectorAll('.themebar .seg button')) b.classList.toggle('on', b.dataset.theme===(t||'auto')); }
(function initTheme(){ const q=new URLSearchParams(location.search).get('theme');
  const cur=q||localStorage.getItem('theme')||'auto';
  document.addEventListener('DOMContentLoaded',()=>{
    for(const b of document.querySelectorAll('.themebar .seg button')){ b.onclick=()=>applyTheme(b.dataset.theme); }
    applyTheme(cur);
  });})();
// Sidebar collapse to an icon rail (persisted).
document.addEventListener('DOMContentLoaded',()=>{
  const rt=document.getElementById('railToggle'); if(!rt)return;
  rt.onclick=()=>{ const on=document.documentElement.dataset.rail==='1';
    if(on){ delete document.documentElement.dataset.rail; localStorage.removeItem('rail'); rt.title='Collapse menu'; }
    else { document.documentElement.dataset.rail='1'; localStorage.setItem('rail','1'); rt.title='Expand menu'; } };
});
"##;

/// Material Symbols (Outlined) glyph name for a UI key. The self-hosted icon font renders these as
/// ligatures via `<span class="material-symbols-outlined">{glyph}</span>`.
fn glyph(key: &str) -> &'static str {
    match key {
        "overview" => "dashboard",
        "browse" => "folder_open",
        "duplicates" => "content_copy",
        "drives" => "hard_drive",
        "scan" => "frame_inspect",
        "console" => "terminal",
        "auto" => "brightness_auto",
        "light" => "light_mode",
        "dark" => "dark_mode",
        _ => "circle",
    }
}

struct NavItem {
    key: &'static str,
    href: &'static str,
    label: &'static str,
}
const NAV: &[NavItem] = &[
    NavItem {
        key: "overview",
        href: "/",
        label: "Overview",
    },
    NavItem {
        key: "browse",
        href: "/browse",
        label: "Browse",
    },
    NavItem {
        key: "duplicates",
        href: "/review",
        label: "Duplicates",
    },
    NavItem {
        key: "drives",
        href: "/drives",
        label: "Drives",
    },
    NavItem {
        key: "scan",
        href: "/scan",
        label: "Scan",
    },
    NavItem {
        key: "console",
        href: "/console",
        label: "Console",
    },
];

/// Render a full self-contained page. `active` is a NAV key.
pub fn shell(active: &str, csrf: &str, title: &str, main_html: &str, page_script: &str) -> String {
    let nav = NAV.iter().map(|n| {
        let cls = if n.key == active { "active" } else { "" };
        let current = if n.key == active { r#" aria-current="page""# } else { "" };
        format!(r#"<a class="{cls}"{current} href="{}" title="{}"><span class="material-symbols-outlined">{}</span><span class="lb">{}</span></a>"#, n.href, n.label, glyph(n.key), n.label)
    }).collect::<String>();
    let themebar = format!(
        r##"<div class="themebar"><span class="lbl">Theme</span>
<div class="seg" role="group" aria-label="Theme">
<button data-theme="auto" title="Follow system"><span class="material-symbols-outlined">{}</span><span class="lb">Auto</span></button>
<button data-theme="light" title="Light"><span class="material-symbols-outlined">{}</span><span class="lb">Light</span></button>
<button data-theme="dark" title="Dark"><span class="material-symbols-outlined">{}</span><span class="lb">Dark</span></button></div></div>"##,
        glyph("auto"),
        glyph("light"),
        glyph("dark")
    );
    format!(
        r##"<!doctype html><html lang="en"><head>
<script>(function(){{var q=new URLSearchParams(location.search);var u=q.get('theme');var t=u||localStorage.getItem('theme');if(t&&t!=='auto')document.documentElement.dataset.theme=t;var r=q.get('rail');var rr=r!=null?r:localStorage.getItem('rail');if(rr==='1')document.documentElement.dataset.rail='1';}})();</script>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="csrf" content="{csrf}"><title>CleanUpStorages — {title}</title>
<style>{STYLE}</style></head><body>
<aside class="side"><div class="side-head"><div class="brand"><h1>CleanUpStorages</h1><p class="tagline">Storage cleanup</p></div>
<button class="rail-toggle" id="railToggle" title="Collapse menu" aria-label="Collapse menu"><span class="material-symbols-outlined">chevron_left</span></button></div>
<nav>{nav}</nav>{themebar}</aside>
<main>{main_html}</main>
<script>{SHARED_JS}</script><script>{page_script}</script>
</body></html>"##
    )
}

pub fn overview_page(csrf: &str) -> String {
    let main = r##"
<section class="card hero">
  <div class="hero-glow"></div>
  <div class="hero-label">System health</div>
  <h2 id="hero" class="hero-stat">…</h2>
  <div class="mut" id="hero-sub" style="font-size:13px"></div>
</section>
<div class="grid">
  <div class="card" style="grid-column:span 5;display:flex;flex-direction:column">
    <div class="row" style="justify-content:space-between;margin-bottom:34px">
      <div class="card-ico"><span class="material-symbols-outlined">content_copy</span></div>
      <span class="tag" id="dupe-tag" style="display:none">High Priority</span>
    </div>
    <div id="dupe-count" style="font-size:26px;font-weight:600;letter-spacing:-.02em;margin:0">…</div>
    <div class="mut" id="dupe-reclaim" style="font-size:13px;margin:6px 0 18px"></div>
    <a class="btn btn-primary" href="/review" style="margin-top:auto;text-decoration:none;width:100%">Review duplicates</a>
  </div>
  <div class="card" style="grid-column:span 7">
    <div class="row" style="justify-content:space-between;margin-bottom:6px">
      <h3 style="margin:0">Reclaimable space</h3>
      <button class="linkbtn" id="purge-link"><span class="material-symbols-outlined">delete_sweep</span>Purge quarantine</button>
    </div>
    <div id="reclaim-bars"></div>
  </div>
  <div class="card" style="grid-column:span 12"><h3 style="margin:0 0 6px">Recent activity</h3>
    <div id="activity" class="mut">Loading…</div></div>
</div>
<div class="mut" id="msg" style="margin-top:4px;min-height:1.2em;text-align:center"></div>"##;
    let script = r##"
const ACT_ICO={scan:["frame_inspect","tone-gray"],quarantine:["inventory_2","tone-red"],
  quarantine_skip:["shield","tone-gray"],quarantine_error:["error","tone-red"],
  repack:["deployed_code","tone-blue"],purge:["delete_sweep","tone-red"],
  forget:["hard_drive","tone-gray"],rename:["edit","tone-gray"]};
async function init(){
  const st=await apiGet("/api/stats");
  const totalFiles=st.volumes.reduce((a,v)=>a+v.active_files,0);
  $("#hero").innerHTML=totalFiles.toLocaleString()+' <span style="font-weight:500;font-size:.5em;color:var(--mut)">files catalogued</span>';
  $("#hero-sub").innerHTML='<span style="color:var(--fg)">Across '+st.volumes.length+" drive"+(st.volumes.length===1?"":"s")+'</span> · catalog stored safely on this computer';
  const g=st.duplicate_groups;
  $("#dupe-count").textContent=g.toLocaleString()+" duplicate group"+(g===1?"":"s");
  if(g>0)$("#dupe-tag").style.display="";
  const drives=await apiGet("/api/drives");
  const totalReclaim=drives.reduce((a,d)=>a+(d.reclaimable_bytes||0),0);
  $("#dupe-reclaim").textContent="≈ "+fmtSize(totalReclaim)+" reclaimable";
  const max=Math.max(1,...drives.map(d=>d.reclaimable_bytes||0));
  $("#reclaim-bars").innerHTML=drives.length?drives.map(d=>`<div style="margin:14px 0 2px">
     <div style="display:flex;justify-content:space-between;font-size:13px;margin-bottom:6px"><span>${esc(d.display_name||d.label)}</span><span class="mono">${fmtSize(d.reclaimable_bytes)}</span></div>
     <div class="progressbar"><span style="width:${Math.round(100*(d.reclaimable_bytes||0)/max)}%"></span></div></div>`).join(""):'<span class="mut">No drives catalogued yet.</span>';
  const SUBS={quarantine:"Flagged as duplicate",quarantine_skip:"Protected the last copy",quarantine_error:"Action failed",
    repack:"Archive optimized",purge:"Space reclaimed",forget:"Removed from catalog",rename:"Details updated"};
  function actParts(a){ const s=a.summary||""; const i=s.indexOf(" — ");
    if(i>=0) return [s.slice(0,i), s.slice(i+3)]; return [s, SUBS[a.kind]||""]; }
  const acts=await apiGet("/api/activity");
  $("#activity").innerHTML=acts.length?acts.map(a=>{
     const[ic,tone]=ACT_ICO[a.kind]||["bolt","tone-gray"]; const[title,sub]=actParts(a);
     return `<div class="actrow"><div class="act-ico ${tone}"><span class="material-symbols-outlined">${ic}</span></div>
       <div style="flex:1;min-width:0"><div class="act-title">${esc(title)}</div>${sub?`<div class="mut" style="font-size:12px;margin-top:1px">${esc(sub)}</div>`:""}</div>
       <div class="act-time">${fmtAgo(a.occurred_at)}</div></div>`;}).join(""):'<div class="mut" style="padding:8px 0">No activity yet.</div>';
}
$("#purge-link").onclick=async()=>{
  if(!window.confirm("Permanently delete every drive's _ToDelete quarantine? This is the only real delete and cannot be undone."))return;
  try{ const r=await apiPost("/api/purge-all",{});
    let m=`Purged ${r.purged_volumes} volume(s), reclaimed ${fmtSize(r.bytes_reclaimed)}.`;
    if(r.skipped_unmounted.length)m+=" Skipped (offline): "+r.skipped_unmounted.join(", ")+".";
    if(r.errors.length)m+=" Errors: "+r.errors.join("; ");
    $("#msg").textContent=m; init(); }
  catch(e){ $("#msg").textContent="Error: "+e; }
};
init().catch(e=>{$("#activity").textContent="Error: "+e;});"##;
    shell("overview", csrf, "Overview", main, script)
}

pub fn browse_page(csrf: &str) -> String {
    let main = r##"
<div class="narrow">
  <div class="searchbar"><span class="material-symbols-outlined">search</span>
    <input id="q" type="search" placeholder="Search filename or path…" autofocus></div>
  <div class="browsetools">
    <select id="volume"><option value="">All drives</option></select>
    <select id="category"><option value="">All types</option>
      <option value="photo">Photos</option><option value="video">Videos</option>
      <option value="document">Documents</option><option value="academic">Academic</option><option value="other">Other</option></select>
    <select id="status"><option value="">Any status</option>
      <option value="active">Active</option><option value="missing">Missing</option>
      <option value="quarantined">Quarantined</option><option value="purged">Purged</option></select>
    <button type="button" class="linkbtn" id="rangetoggle" aria-expanded="false">Size &amp; date</button>
    <span class="mut count" id="count"></span>
  </div>
  <div class="browsetools" id="rangerow" hidden>
    <label class="rangelbl">Size
      <select id="minsize"><option value="">any</option>
        <option value="1048576">≥ 1 MB</option><option value="10485760">≥ 10 MB</option>
        <option value="104857600">≥ 100 MB</option><option value="1073741824">≥ 1 GB</option></select>
      <select id="maxsize"><option value="">any</option>
        <option value="1048576">≤ 1 MB</option><option value="10485760">≤ 10 MB</option>
        <option value="104857600">≤ 100 MB</option><option value="1073741824">≤ 1 GB</option></select>
    </label>
    <label class="rangelbl">Modified
      <input type="date" id="datefrom" aria-label="Modified on or after">
      <span class="mut">to</span>
      <input type="date" id="dateto" aria-label="Modified on or before">
    </label>
    <button type="button" class="linkbtn" id="clearranges">Clear</button>
  </div>
  <div class="mut" id="datenote" style="font-size:12px;margin:0 0 8px" hidden>Files inside archives use the date of the archive they're in. Any catalogued before this feature need a rescan to pick that up.</div>
  <div class="mut" style="font-size:12px;margin:0 0 10px">Grouped by drive and folder. Files sharing identical content share a <span class="diamond" style="--dup:hsl(280,72%,52%)">◆</span> color — click one to highlight every copy.</div>
  <div class="card" id="copies" hidden style="margin-bottom:12px;padding:12px 14px"></div>
  <div class="card tree-card" style="padding:8px"><div id="results" class="tree"></div></div>
</div>"##;
    let script = r##"
let timer=null;
// Multi-select filter state: each is an array of chosen values (empty = no filter). Sent to the API
// comma-joined, OR-combined server-side.
const F={volume:[],category:[],status:[]};
// --- data -> tree model (pure) ---
function buildTree(hits){
  const drives=new Map();
  for(const h of hits){
    if(!drives.has(h.volume_id)) drives.set(h.volume_id,{id:h.volume_id,label:h.volume_label||h.volume_id,size:0,children:new Map(),files:[]});
    const drive=drives.get(h.volume_id); drive.size+=(h.size_bytes||0);
    // Quarantined/purged rows live under _ToDelete on disk, but we show them at their last valid
    // location (original_path) so the tree never grows a _ToDelete branch.
    const loc = (h.original_path && !h.container_chain) ? h.original_path : h.relative_path;
    const segs=String(loc||"").split('/').filter(Boolean);
    const last=segs.pop()||h.filename||"(file)";
    let node=drive;
    for(const seg of segs){ if(!node.children.has(seg)) node.children.set(seg,{name:seg,children:new Map(),files:[]}); node=node.children.get(seg); }
    if(h.container_chain){
      if(!node.children.has(last)) node.children.set(last,{name:last,archive:true,children:new Map(),files:[]});
      node.children.get(last).files.push({name:h.container_chain,hit:h});
    } else { node.files.push({name:last,hit:h}); }
  }
  for(const d of drives.values()) reconcile(d);
  return drives;
}
// A .zip catalogued both as a loose file AND via its entries would otherwise show twice. Fold the
// loose archive file's own size onto its archive node and drop the duplicate leaf.
function reconcile(node){
  const keep=[];
  for(const f of node.files){ const a=node.children.get(f.name); if(a&&a.archive){ a.selfHit=f.hit; } else keep.push(f); }
  node.files=keep;
  for(const c of node.children.values()) reconcile(c);
}
function countDups(node){ let n=node.files.filter(f=>f.hit.copies).length; for(const c of node.children.values()) n+=countDups(c); return n; }
// --- model -> HTML (pure; every interpolation esc()'d) ---
function fileGlyph(name){
  const e=(String(name).split('.').pop()||'').toLowerCase();
  if(['jpg','jpeg','png','gif','webp','heic','heif','bmp','tif','tiff','svg','raw','cr2','nef'].includes(e))return 'image';
  if(['mp4','mov','avi','mkv','webm','m4v','wmv','flv'].includes(e))return 'movie';
  if(['mp3','wav','flac','aac','ogg','m4a','aiff'].includes(e))return 'audio_file';
  if(['zip','rar','7z','tar','gz','bz2','xz'].includes(e))return 'folder_zip';
  if(e==='pdf')return 'picture_as_pdf';
  if(['doc','docx','txt','md','rtf','pages','odt'].includes(e))return 'description';
  if(['xls','xlsx','csv','numbers','ods'].includes(e))return 'table_chart';
  if(['ppt','pptx','key','odp'].includes(e))return 'slideshow';
  if(['js','ts','rs','py','html','css','json','c','cpp','h','java','go','sh','rb','php'].includes(e))return 'code';
  return 'draft';
}
function renderLeaf(f){
  const h=f.hit;
  const dia=h.copies?`<span class="diamond" style="--dup:${dupColor(h.content_hash)}" title="${h.copies} copies">◆${h.copies}</span>`:"";
  const pill=h.status!=="active"?`<span class="pill ${esc(h.status)}">${esc(h.status)}</span>`:"";
  const cls=h.copies?"leaf dup":"leaf";
  const style=h.copies?`style="--dup:${dupColor(h.content_hash)}"`:"";
  return `<div class="${cls}" data-hash="${esc(h.content_hash)}" ${style}><span class="material-symbols-outlined fico">${fileGlyph(f.name)}</span><span class="fname">${esc(f.name)}</span>${dia}<span class="meta mut">${fmtSize(h.size_bytes)}</span>${pill}</div>`;
}
function renderFolder(node){
  let html='<div class="branch">';
  for(const child of node.children.values()){
    const d=countDups(child); const badge=d?` <span class="mut" style="font-size:11px">${d} dup</span>`:"";
    const ico=`<span class="material-symbols-outlined fico" style="font-size:17px!important">${child.archive?'folder_zip':'folder'}</span>`;
    const sz=child.archive&&child.selfHit?` <span class="mut mono" style="font-size:11px">${fmtSize(child.selfHit.size_bytes)}</span>`:'';
    html+=`<details class="folder"><summary>${ico}${esc(child.name)}${badge}${sz}</summary>${renderFolder(child)}</details>`;
  }
  for(const f of node.files) html+=renderLeaf(f);
  return html+'</div>';
}
function renderTree(drives){
  if(!drives.size) return '<div class="mut" style="padding:12px">No files match.</div>';
  let html="";
  for(const drive of drives.values()){
    html+=`<details class="drive" open><summary><span class="dot" style="background:${driveColor(drive.id)}"></span><b>${esc(drive.label)}</b> <span class="mut">${fmtSize(drive.size)}</span></summary>${renderFolder(drive)}</details>`;
  }
  return html;
}
// A picked date is a LOCAL calendar day, so "to" means the end of that day, inclusive — otherwise
// choosing the same day for both bounds would match only midnight.
function dayEpoch(value,endOfDay){
  if(!value) return null;
  const [y,m,d]=value.split("-").map(Number);
  if(!y||!m||!d) return null;
  const dt=endOfDay ? new Date(y,m-1,d,23,59,59) : new Date(y,m-1,d,0,0,0);
  return Math.floor(dt.getTime()/1000);
}
function rangeParams(p){
  const minS=$("#minsize").value, maxS=$("#maxsize").value;
  if(minS) p.set("min_size",minS);
  if(maxS) p.set("max_size",maxS);
  const from=dayEpoch($("#datefrom").value,false), to=dayEpoch($("#dateto").value,true);
  if(from!==null) p.set("modified_after",String(from));
  if(to!==null) p.set("modified_before",String(to));
  // Archive entries inherit their archive's date, but only from a scan done since that feature
  // landed. Flag it whenever a date bound is active so an under-count isn't mistaken for a bug.
  $("#datenote").hidden = (from===null && to===null);
  return (minS||maxS||from!==null||to!==null);
}
// --- lazy browse: one folder level at a time, straight from the catalogue ---
//
// The tree is NOT reconstructed from a search result. Doing that summed each folder from whatever
// rows the page had loaded and dropped any drive with nothing in the slice -- on the real
// catalogue the 4.75 TB drive vanished, because 39,171 rows from the other drive sorted ahead of
// its first path, and the drive that did show read 114.6 GB against a real 1.87 TB.
const PAGE=200;
function renderDirRow(volumeId,dir){
  return `<details class="folder" data-vol="${esc(volumeId)}" data-path="${esc(dir.path)}">`
    +`<summary><span class="material-symbols-outlined fico" style="font-size:17px!important">folder</span>`
    +`${esc(dir.name)} <span class="mut mono" style="font-size:11px">${fmtSize(dir.total_bytes)}`
    +` · ${fmtN(dir.file_count)} file${dir.file_count===1?"":"s"}</span></summary>`
    +`<div class="branch"><div class="mut" style="padding:4px 8px">Loading…</div></div></details>`;
}
// Archive entries carry their archive's path, so they arrive in the same folder as the archive
// itself. Group them under one node instead of listing each entry at the folder's top level.
function renderFileRows(files){
  const archives=new Map(); let html="";
  for(const h of files){
    if(!h.container_chain) continue;
    const key=h.relative_path;
    if(!archives.has(key)) archives.set(key,[]);
    archives.get(key).push(h);
  }
  for(const h of files){
    if(h.container_chain) continue;
    const inner=archives.get(h.relative_path);
    if(inner){
      const rows=inner.map(e=>renderLeaf({name:e.container_chain,hit:e})).join("");
      html+=`<details class="folder"><summary><span class="material-symbols-outlined fico" style="font-size:17px!important">folder_zip</span>`
        +`${esc(h.filename)} <span class="mut mono" style="font-size:11px">${fmtSize(h.size_bytes)}</span></summary>`
        +`<div class="branch">${rows}</div></details>`;
      archives.delete(h.relative_path);
      continue;
    }
    html+=renderLeaf({name:h.filename,hit:h});
  }
  // Entries whose archive row itself is not in this folder (never expected, but do not drop files).
  for(const [path,inner] of archives){
    const rows=inner.map(e=>renderLeaf({name:e.container_chain,hit:e})).join("");
    html+=`<details class="folder"><summary><span class="material-symbols-outlined fico" style="font-size:17px!important">folder_zip</span>`
      +`${esc(String(path).split('/').pop())}</summary><div class="branch">${rows}</div></details>`;
  }
  return html;
}
async function loadLevel(volumeId,path,into,offset){
  const p=new URLSearchParams({volume:volumeId,path:path,limit:String(PAGE),offset:String(offset||0)});
  const data=await apiGet("/api/folder?"+p.toString());
  const html=data.dirs.map(d=>renderDirRow(volumeId,d)).join("")+renderFileRows(data.files);
  const more=(data.more_dirs||data.more_files)
    ? `<button type="button" class="linkbtn loadmore" data-vol="${esc(volumeId)}" data-path="${esc(path)}" data-offset="${(offset||0)+PAGE}">Load more</button>`
    : "";
  if(offset){ into.querySelector(".loadmore")?.remove(); into.insertAdjacentHTML("beforeend",html+more); }
  else into.innerHTML=(html||'<div class="mut" style="padding:4px 8px">This folder is empty.</div>')+more;
}
// Drive headers take their totals from the catalogue, never from the rows on screen.
async function renderDrives(){
  const vols=await apiGet("/api/volumes");
  const wanted=F.volume.length?vols.filter(v=>F.volume.includes(v.volume_id)):vols;
  if(!wanted.length){ $("#results").innerHTML='<div class="mut" style="padding:12px">No drives catalogued yet.</div>'; $("#count").textContent=""; return; }
  $("#count").textContent=wanted.length+" drive"+(wanted.length===1?"":"s")
    +" · "+fmtN(wanted.reduce((a,v)=>a+(v.active_files||0),0))+" files";
  $("#results").innerHTML=wanted.map(v=>{
    const name=v.display_name||v.label||v.volume_id;
    return `<details class="drive" data-vol="${esc(v.volume_id)}" data-path="">`
      +`<summary><span class="dot" style="background:${driveColor(v.volume_id)}"></span>`
      +`<b>${esc(name)}</b> <span class="mut">${fmtSize(v.active_bytes)} · ${fmtN(v.active_files)} files</span></summary>`
      +`<div class="branch"><div class="mut" style="padding:4px 8px">Loading…</div></div></details>`;
  }).join("");
}
// One fetch per folder, the first time it is opened.
$("#results").addEventListener("toggle",async e=>{
  const el=e.target;
  if(!(el instanceof HTMLDetailsElement)||!el.open||el.dataset.loaded) return;
  if(el.dataset.vol===undefined) return;
  el.dataset.loaded="1";
  const box=el.querySelector(":scope > .branch");
  try{ await loadLevel(el.dataset.vol,el.dataset.path||"",box,0); }
  catch(err){ box.innerHTML='<div class="mut" style="padding:4px 8px">Could not load this folder: '+esc(String(err))+'</div>'; delete el.dataset.loaded; }
},true);
$("#results").addEventListener("click",async e=>{
  const btn=e.target.closest(".loadmore"); if(!btn) return;
  const box=btn.parentElement; const off=Number(btn.dataset.offset)||0;
  btn.textContent="Loading…"; btn.disabled=true;
  try{ await loadLevel(btn.dataset.vol,btn.dataset.path,box,off); }
  catch(err){ btn.disabled=false; btn.textContent="Load more — "+String(err); }
});
async function run(){ try{
  const p=new URLSearchParams(); const q=$("#q").value.trim(); if(q)p.set("q",q);
  for(const k of ["volume","category","status"]){ if(F[k].length) p.set(k,F[k].join(",")); }
  const ranged=rangeParams(p);
  $("#rangetoggle").classList.toggle("has-sel",ranged);
  // Nothing to search for: show the drives and let the user walk into them. Any query or filter is
  // a real search, and a search result legitimately is a capped, flat set of matches.
  if(!q && !ranged && !F.category.length && !F.status.length){ await renderDrives(); return; }
  p.set("limit","3000");
  const hits=await apiGet("/api/search?"+p.toString());
  $("#count").textContent=hits.length+" result"+(hits.length===1?"":"s")+(hits.length>=3000?" (showing first 3000 — refine your search)":"");
  $("#results").innerHTML=renderTree(buildTree(hits));
}catch(e){ $("#count").textContent="Search error: "+e; } }
// click a duplicated leaf -> highlight every visible copy (same content hash)
$("#results").addEventListener("click",e=>{
  const leaf=e.target.closest(".leaf.dup"); if(!leaf)return;
  const hash=leaf.dataset.hash, was=leaf.classList.contains("hl");
  document.querySelectorAll(".leaf.hl").forEach(el=>el.classList.remove("hl"));
  if(was){ $("#copies").hidden=true; return; }
  document.querySelectorAll('.leaf[data-hash="'+CSS.escape(hash)+'"]').forEach(el=>el.classList.add("hl"));
  showCopies(hash);
});
// The highlight can only mark rows this page loaded, and the page is capped. Ask the catalogue
// where the copies actually are, or a truncated result set quietly under-reports them (#30).
async function showCopies(hash){
  const box=$("#copies"); box.hidden=false;
  box.innerHTML='<span class="mut">Looking for every copy…</span>';
  try{
    const cs=await apiGet("/api/copies?hash="+encodeURIComponent(hash));
    const onPage=document.querySelectorAll('.leaf[data-hash="'+CSS.escape(hash)+'"]').length;
    const rows=cs.map(c=>{
      const where=c.mounted?"":' <span class="mut">(drive not connected)</span>';
      const st=c.status!=="active"?` <span class="pill ${esc(c.status)}">${esc(c.status)}</span>`:"";
      return `<div class="copyrow"><span class="mono">${esc(c.location)}</span>
        <span class="mut">${esc(c.volume_label||c.volume_id)}${where}</span>${st}</div>`;
    }).join("");
    const hidden=cs.length-onPage;
    const note=hidden>0
      ? `<span class="mut">${hidden} of them ${hidden===1?"is":"are"} not shown in the tree above.</span>`
      : "";
    box.innerHTML=`<div class="copyhead"><b>${cs.length}</b> ${cs.length===1?"copy":"copies"} of this content ${note}</div>${rows}`;
  }catch(err){ box.innerHTML='<span class="mut">Could not load the copy list: '+esc(String(err))+'</span>'; }
}
function debounced(){ clearTimeout(timer); timer=setTimeout(run,180); }
// Turn a native <select> into a styled MULTI-select dropdown. The chosen values live in F[sel.id]
// (an array; empty = no filter). `opts.badge(value)` may return a count to flag on each option,
// refreshed whenever the menu opens. `opts.onChange()` fires after every toggle.
function enhanceSelect(sel,opts){
  opts=opts||{}; const key=sel.id, badge=opts.badge, onChange=opts.onChange||(()=>{});
  const placeholder=(sel.options[0]&&sel.options[0].value==="")?sel.options[0].textContent:"All";
  const choices=[...sel.options].filter(o=>o.value!=="");
  const sel_=new Set(F[key]||[]);
  const dd=document.createElement("div"); dd.className="dd multi";
  const btn=document.createElement("button"); btn.type="button"; btn.className="dd-btn"; btn.setAttribute("aria-haspopup","listbox");
  const lab=document.createElement("span"); lab.className="dd-label";
  const car=document.createElement("span"); car.className="material-symbols-outlined dd-caret"; car.textContent="expand_more";
  btn.append(lab,car);
  const menu=document.createElement("div"); menu.className="dd-menu"; menu.hidden=true; menu.setAttribute("role","listbox");
  function labelText(){ if(sel_.size===0) return placeholder;
    if(sel_.size===1){ const v=[...sel_][0]; const o=choices.find(c=>c.value===v); return o?o.textContent:v; }
    return sel_.size+" selected"; }
  function sync(){ lab.textContent=labelText(); dd.classList.toggle("has-sel",sel_.size>0);
    for(const o of menu.children) o.classList.toggle("sel", o.dataset.value!=null && sel_.has(o.dataset.value)); }
  function commit(){ F[key]=[...sel_]; sync(); onChange(); }
  function refreshBadges(){ if(!badge)return;
    for(const o of menu.children){ if(o.dataset.value==null)continue; const b=badge(o.dataset.value); const el=o.querySelector(".dd-badge");
      if(el){ el.textContent=b==null?"":String(b); o.classList.toggle("zero", b===0); } } }
  for(const opt of choices){
    const b=document.createElement("button"); b.type="button"; b.className="dd-opt"; b.dataset.value=opt.value; b.setAttribute("role","option");
    b.innerHTML='<span class="dd-box"><span class="material-symbols-outlined dd-ck">check</span></span><span class="dd-t">'+esc(opt.textContent)+'</span><span class="dd-badge"></span>';
    b.onclick=e=>{ e.stopPropagation(); if(sel_.has(opt.value))sel_.delete(opt.value); else sel_.add(opt.value); commit(); };
    menu.appendChild(b);
  }
  const clear=document.createElement("button"); clear.type="button"; clear.className="dd-clear";
  clear.textContent="Clear"; clear.onclick=e=>{ e.stopPropagation(); sel_.clear(); commit(); };
  menu.appendChild(clear);
  function open(){ for(const m of document.querySelectorAll(".dd.open")) if(m!==dd){m.classList.remove("open");m.querySelector(".dd-menu").hidden=true;} refreshBadges(); menu.hidden=false; dd.classList.add("open"); btn.setAttribute("aria-expanded","true"); }
  function close(){ menu.hidden=true; dd.classList.remove("open"); btn.setAttribute("aria-expanded","false"); }
  btn.onclick=e=>{ e.stopPropagation(); menu.hidden?open():close(); };
  document.addEventListener("click",e=>{ if(!dd.contains(e.target)) close(); });
  document.addEventListener("keydown",e=>{ if(e.key==="Escape") close(); });
  sel.parentNode.insertBefore(dd,sel); dd.append(btn,menu,sel); sel.style.display="none";
  sync();
}
let statusCounts={};
async function loadCounts(){
  const p=new URLSearchParams(); const q=$("#q").value.trim(); if(q)p.set("q",q);
  if(F.volume.length)p.set("volume",F.volume.join(",")); if(F.category.length)p.set("category",F.category.join(","));
  try{ statusCounts=await apiGet("/api/status-counts?"+p.toString()); }catch(e){ statusCounts={}; }
}
async function init(){
  const vs=await apiGet("/api/volumes"); const sel=$("#volume");
  for(const v of vs){ const o=document.createElement("option"); o.value=v.volume_id; o.textContent=v.display_name||v.label; sel.appendChild(o); }
  const urlq=new URLSearchParams(location.search).get("q"); if(urlq)$("#q").value=urlq;
  $("#q").addEventListener("input",()=>{ debounced(); clearTimeout(window.__ct); window.__ct=setTimeout(loadCounts,180); });
  enhanceSelect($("#volume"),{onChange:()=>{ loadCounts(); run(); }});
  enhanceSelect($("#category"),{onChange:()=>{ loadCounts(); run(); }});
  // Status filter flags how many rows carry each kind (active/missing/quarantined/purged), so
  // hidden-by-default purged rows stay discoverable; toggling combines statuses (OR).
  enhanceSelect($("#status"),{onChange:run, badge: val => (statusCounts[val]||0)});
  // Size/date are ranges, not multi-select, so they stay plain controls (enhanceSelect is the
  // multi-select widget and would misrepresent them).
  $("#rangetoggle").addEventListener("click",()=>{
    const row=$("#rangerow"), open=row.hidden;
    row.hidden=!open;
    $("#rangetoggle").setAttribute("aria-expanded",String(open));
  });
  for(const id of ["minsize","maxsize","datefrom","dateto"]) $("#"+id).addEventListener("change",run);
  $("#clearranges").addEventListener("click",()=>{
    for(const id of ["minsize","maxsize","datefrom","dateto"]) $("#"+id).value="";
    run();
  });
  await loadCounts();
  run();
}
init();"##;
    shell("browse", csrf, "Browse", main, script)
}

pub fn review_page(csrf: &str) -> String {
    let main = r##"
<div class="rv-page">
  <div class="row" style="justify-content:space-between;align-items:baseline;margin-bottom:6px">
    <h1 class="page-h" style="margin:0">Review duplicates</h1><span class="mut" id="progress"></span></div>
  <div class="row" style="justify-content:space-between;align-items:center;flex-wrap:wrap;gap:12px;margin-bottom:18px">
    <div class="mut" id="totals" style="font-size:13px;line-height:1.5"></div>
    <label class="row" style="font-size:13px;color:var(--mut);gap:8px;white-space:nowrap">Only show files ≥
      <select id="minsize">
        <option value="0">any size</option>
        <option value="65536">64 KB</option>
        <option value="1048576" selected>1 MB</option>
        <option value="10485760">10 MB</option>
        <option value="104857600">100 MB</option>
      </select></label>
  </div>
  <section id="treesec" style="display:none;margin-bottom:26px">
    <h2 style="font-size:15px;margin:0 0 4px">Identical folders</h2>
    <p class="mut" style="font-size:12px;margin:0 0 12px">Whole folders whose contents match exactly.
      Confirming one moves the entire folder to <span class="mono">_ToDelete</span> in a single
      rename — the other copy stays where it is, and nothing is deleted until you purge.</p>
    <div class="mut" id="qstatus" style="display:none;font-size:12.5px;margin-bottom:10px"></div>
  <div id="treelist"></div>
  <div id="treeblocked"></div>
  </section>
  <div id="group"></div>
  <div class="mut" id="upnext" style="font-size:12px;margin-top:14px;text-align:center"></div>
  <div class="rvbar">
    <button class="linkbtn" id="skip" style="font-size:13px">Skip this group</button>
    <button class="btn btn-primary" id="confirm">Keep selected, quarantine the rest</button>
  </div>
  <p class="mut" style="font-size:11.5px;text-align:center;margin-top:16px">Nothing is deleted — copies move to a recoverable <span class="mono">_ToDelete</span> folder until you purge.</p>
  <div class="mut" id="msg" style="margin-top:10px;min-height:1.4em;text-align:center"></div>
</div>"##;
    let script = r##"
let groups=[],idx=0,keepId=null,totals=null,next=null,minSize=1048576,exhausted=false;
// One ranked page at a time: the catalogue can hold hundreds of thousands of groups.
async function loadPage(reset){
  if(reset){ groups=[]; idx=0; next=null; exhausted=false; }
  const p=new URLSearchParams({min_size:String(minSize),limit:"50"});
  if(next){ p.set("after_reclaimable",String(next.reclaimable_bytes)); p.set("after_hash",next.content_hash); }
  const r=await apiGet("/api/duplicates?"+p.toString());
  if(r.totals) totals=r.totals;   // continuation pages omit them; keep what we have
  if(!r.groups.length) exhausted=true;
  groups=groups.concat(r.groups); next=r.next;
  paintTotals();
}
// The size filter must never look like it changed how much space you can reclaim, so the headline
// is floor-free and what the filter hides is always spelled out.
function paintTotals(){
  if(!totals)return;
  const hiddenGroups=totals.groups_all-totals.groups, hiddenBytes=totals.reclaimable_all_bytes-totals.reclaimable_bytes;
  let s=`<b>${fmtN(totals.groups)}</b> groups · <b>${fmtSize(totals.reclaimable_bytes)}</b> reclaimable`;
  if(totals.archive_locked_bytes>0) s+=` · ${fmtSize(totals.archive_locked_bytes)} locked inside archives (needs repack)`;
  if(hiddenGroups>0) s+=`<br><span style="opacity:.75">${fmtN(hiddenGroups)} smaller groups (${fmtSize(hiddenBytes)}) hidden by this filter — lower it to review them.</span>`;
  $("#totals").innerHTML=s;
}
async function load(){
  try{ await loadPage(true); }catch(e){ $("#msg").textContent="Load error: "+e; return; }
  render();
}
function render(){
  if(idx>=groups.length){
    if(next&&!exhausted){ loadPage(false).then(render).catch(e=>{$("#msg").textContent="Load error: "+e;}); return; }
    $("#progress").textContent=""; $("#upnext").textContent="";
    $("#group").innerHTML='<div class="empty"><h2 style="margin:0 0 6px">All duplicate groups reviewed 🎉</h2><p class="mut">Nothing left to compare at this size filter.</p></div>';
    $("#confirm").style.display="none"; $("#skip").style.display="none"; return;
  }
  $("#confirm").style.display=""; $("#skip").style.display="";
  const g=groups[idx]; keepId=g.suggested_keep_id;
  $("#progress").textContent=`Group ${idx+1} of ${fmtN(totals?totals.groups:groups.length)} · ${g.members.length} copies · ${fmtSize(g.reclaimable_bytes)} reclaimable`;
  $("#group").innerHTML=`<div class="cards">${g.members.map(m=>card(m)).join("")}</div>`;
  for(const el of document.querySelectorAll(".cards .card")) el.addEventListener("click",()=>{ keepId=Number(el.dataset.id); paint(); });
  paint();
  for(const b of document.querySelectorAll(".repack")) b.addEventListener("click", async (ev)=>{
    ev.stopPropagation();
    const id=Number(b.dataset.id);
    b.disabled=true; $("#msg").textContent="Repacking archive…";
    try{
      const j=await apiPost("/api/repack",{entry_id:id});
      $("#msg").textContent=`Removed '${j.removed_entry}' from its archive (${j.retained_entries} kept). Original saved in _ToDelete.`; idx++; render();
    }catch(e){ $("#msg").textContent="Repack error: "+e; b.disabled=false; }
  });
  const rest=groups.slice(idx+1,idx+4);
  $("#upnext").textContent=rest.length?"Up next: "+rest.map(x=>`${fmtSize(x.reclaimable_bytes)} (${x.copies} copies)`).join(" · "):"";
}
function card(m){
  const img=(m.category==="photo"&&m.mounted)?`<img class="thumb" loading="lazy" src="/api/preview/${m.id}" onerror="this.replaceWith(Object.assign(document.createElement('div'),{className:'noimg',textContent:'no preview'}))">`:`<div class="noimg">${m.mounted?"no preview":"drive not connected"}</div>`;
  const arch = m.is_loose ? "" :
    (m.id===keepId ? `<div class="arch">inside archive</div>`
     : m.mounted ? `<button class="btn repack" data-id="${m.id}" style="width:100%;margin-top:12px">Remove from archive</button>`
                 : `<div class="arch">drive not connected</div>`);
  const created=m.created_time?`<div class="dl"><span class="k">Created</span><span class="v">${fmtDate(m.created_time)}</span></div>`:"";
  return `<div class="card rvcard" data-id="${m.id}">
    <div class="rvthumb">${img}<span class="keep-pill kept-badge" style="visibility:hidden"><span class="material-symbols-outlined">check_circle</span>Keep this</span></div>
    <div class="rvbody">
      <div class="rvpath">${esc(m.location)}</div>
      <div class="dl"><span class="k">Drive</span><span class="v">${esc(m.volume_label||m.volume_id)}</span></div>
      <div class="dl"><span class="k">Size</span><span class="v mono">${fmtSize(m.size_bytes)}</span></div>
      ${created}
      <div class="dl"><span class="k">Status</span><span class="v">${esc(m.status)}</span></div>${arch}
    </div></div>`;
}
function paint(){
  for(const el of document.querySelectorAll(".cards .card")){
    const on=Number(el.dataset.id)===keepId;
    el.classList.toggle("keep",on);
    el.querySelector(".kept-badge").style.visibility=on?"visible":"hidden";
  }
}
$("#confirm").addEventListener("click",async()=>{
  const g=groups[idx]; if(!g)return;
  const victims=g.members.filter(m=>m.id!==keepId&&m.is_loose).map(m=>m.id);
  if(victims.length===0){ $("#msg").textContent="Nothing to quarantine (the other copies are inside archives)."; return; }
  // The request only ENQUEUES, so the reviewer moves to the next group straight away instead of
  // waiting out a re-hash. Failures surface through the status poll, not from this call.
  try{
    const j=await apiPost("/api/quarantine",{quarantine_ids:victims});
    let m="Queued "+j.queued+" file"+(j.queued===1?"":"s")+".";
    if(j.skipped) m+=" "+j.skipped+" skipped.";
    if(j.unmounted_volumes&&j.unmounted_volumes.length) m+=" Some drives not connected.";
    $("#msg").textContent=m;
    idx++; render();
    pollQuarantine();
  }catch(e){ $("#msg").textContent="Could not queue: "+e; }
});
$("#skip").addEventListener("click",()=>{ idx++; $("#msg").textContent=""; render(); });
// A plain <select>: enhanceSelect is the Browse page's multi-select widget and the floor is one value.
$("#minsize").addEventListener("change",()=>{ minSize=Number($("#minsize").value); $("#msg").textContent=""; load(); });

// ---- Identical folders (#38) ----------------------------------------------------------------
// Shown ABOVE the per-file list because it is the decision worth making first: on a real catalogue
// this turns ~126,000 individual choices into ~1,500.
const fmtB=n=>{const u=['B','KB','MB','GB','TB'];let i=0,x=Number(n)||0;
  while(x>=1024&&i<u.length-1){x/=1024;i++;} return (i===0?x:x.toFixed(1))+' '+u[i];};
let treeOffset=0, treeTotal=0;
async function loadTrees(more){
  // Batched, not all at once: the full set is ~2.6 MB and thousands of rows, and the top of the
  // list carries nearly all the reclaimable space. Fetch a page, render it, and ask for the next
  // one only if the user actually wants it.
  if(!more) treeOffset=0;
  let data; try{
    data=await apiGet("/api/tree-duplicates?limit=100&offset="+treeOffset);
  }catch(e){ return; }
  treeTotal=data.total;
  const sec=$("#treesec"), host=$("#treelist"), blocked=$("#treeblocked");
  if(!data.groups.length && !more){ sec.style.display="none"; return; }
  sec.style.display="";
  if(!more){ host.textContent=""; blocked.textContent=""; }
  treeOffset += data.groups.length;
  // Split rather than sort-and-hope. On the real catalogue 1,273 of 2,792 groups have every copy
  // inside an archive, so they cannot be quarantined at all; leaving them interleaved buries the
  // 1,519 you can act on. They stay visible, with the reason, but out of the way (#59).
  const act=data.groups.filter(g=>g.actionable), lock=data.groups.filter(g=>!g.actionable);
  const lockBytes=lock.reduce((a,g)=>a+g.reclaimable_bytes,0);
  if(lock.length){
    const d=document.createElement("details");
    d.style.cssText="margin-top:14px";
    const sum=document.createElement("summary");
    sum.style.cssText="cursor:pointer;font-size:13px;color:var(--mut)";
    sum.textContent=lock.length.toLocaleString()+" more folders are duplicated only inside archives ("
      +fmtB(lockBytes)+") — these need a repack, not a move";
    d.appendChild(sum);
    const inner=document.createElement("div");
    inner.style.cssText="margin-top:10px";
    d.appendChild(inner);
    blocked.appendChild(d);
    renderGroups(lock, inner, false);
  }
  renderGroups(act, host, !!more);

  // A "Load more" button rather than infinite scroll: this is a worklist, and a control the user
  // presses is easier to reason about than rows appearing while they read a path they may be about
  // to delete.
  const old=document.getElementById("treemore"); if(old) old.remove();
  if(treeOffset < treeTotal){
    const b=document.createElement("button");
    b.id="treemore"; b.className="linkbtn"; b.style.cssText="margin-top:12px;font-size:12.5px";
    b.textContent="Load more ("+(treeTotal-treeOffset).toLocaleString()+" groups left)";
    b.addEventListener("click", async ()=>{ b.disabled=true; b.textContent="Loading...";
      await loadTrees(true); });
    host.parentNode.insertBefore(b, host.nextSibling);
  }
}

function renderGroups(groups, host, append){
  if(!append) host.textContent="";
  for(const g of groups){
    const box=document.createElement("div");
    box.className="card";
    box.style.cssText="padding:12px 14px;margin-bottom:10px";
    const head=document.createElement("div");
    head.style.cssText="font-size:13px;margin-bottom:8px";
    // Blast radius first. The user is about to move thousands of files with one click, and this
    // line is the whole mitigation for making each decision coarser than one file.
    head.innerHTML=`<strong>${fmtN(g.file_count)} files</strong> <span class="mut">· ${
      esc(fmtB(g.reclaimable_bytes))} reclaimable</span>`;
    box.appendChild(head);
    for(const m of g.members){
      const row=document.createElement("div");
      row.className="row";
      row.style.cssText="justify-content:space-between;gap:12px;padding:4px 0;flex-wrap:wrap";
      const label=document.createElement("span");
      label.className="mono";
      label.style.cssText="font-size:12px;word-break:break-all";
      // BOTH full paths, always: folder names are not part of the match, so the path is the only
      // way to tell which copy is which.
      label.textContent=m.volume_label+" / "+m.path;
      row.appendChild(label);
      if(m.needs_repack){
        const b=document.createElement("span");
        b.className="mut"; b.style.fontSize="11.5px";
        b.textContent="inside "+m.archive+" — needs repack";
        row.appendChild(b);
      }else if(!m.mounted){
        const b=document.createElement("span");
        b.className="mut"; b.style.fontSize="11.5px";
        b.textContent="drive not connected";
        row.appendChild(b);
      }else{
        const btn=document.createElement("button");
        btn.className="linkbtn"; btn.style.fontSize="12.5px";
        btn.textContent="Quarantine this copy";
        btn.addEventListener("click",()=>confirmTree(g,m,btn));
        row.appendChild(btn);
      }
      box.appendChild(row);
    }
    host.appendChild(box);
  }
}
async function confirmTree(group,member,btn){
  // The confirm stays per item: the blast radius must be shown before anything is queued. What
  // changed is what happens after -- the request only ENQUEUES, so the reviewer can confirm the
  // next folder straight away instead of waiting out a move of 326,569 files (#66).
  // The line breaks below MUST stay as the two-character escape \n. A real newline inside this
  // string is a JavaScript syntax error, which kills the whole page script -- including the
  // per-file duplicate list, which then silently renders nothing.
  if(!confirm("Move this entire folder to _ToDelete?\n\n"+member.volume_label+" / "+member.path+
      "\n"+fmtN(group.file_count)+" files, "+fmtB(member.total_bytes)+
      "\n\nThe other copy stays where it is. Nothing is deleted until you purge.")) return;
  btn.disabled=true; btn.textContent="Queued";
  try{
    await apiPost("/api/quarantine-tree",{volume_id:member.volume_id,path:member.path});
    pollQuarantine();
  }catch(e){
    // apiPost throws on non-2xx; a rejected REQUEST lands here. Failures from the worker surface
    // through the status poll instead, since by then the request has long since returned.
    $("#msg").textContent="Could not queue: "+e.message;
    btn.disabled=false; btn.textContent="Quarantine this copy";
  }
}

let qTimer=null, qWasBusy=false;
async function pollQuarantine(){
  if(qTimer) return;                       // one poller is enough
  const tick=async()=>{
    let st; try{ st=await apiGet("/api/quarantine/status"); }catch(e){ return; }
    const busy=!!st.running || st.pending.length>0;
    const bar=$("#qstatus");
    if(busy){
      const now=st.running?st.running.label:"";
      bar.textContent="Quarantining "+now+(st.pending.length?" — "+st.pending.length+" queued":"");
      bar.style.display="";
    }else{ bar.style.display="none"; }
    // Worker failures have to reach the user: they clicked, and it did not happen. A tree that is
    // no longer wholly active, or a drive swapped mid-queue, is refused rather than forced.
    const failed=st.recent.filter(r=>r.error_message);
    if(failed.length){
      $("#msg").textContent=failed.length+" could not be quarantined — "+failed[0].error_message;
    }else if(st.recent.length){
      const folders=st.recent.filter(r=>!r.error_message&&r.kind==="tree").length;
      const files=st.recent.filter(r=>!r.error_message&&r.kind==="files")
                           .reduce((a,r)=>a+r.files_updated,0);
      // A guarded skip is not a failure, but hiding it would let the reviewer believe a copy was
      // handled when the last-copy guard deliberately kept it.
      const skipped=st.recent.reduce((a,r)=>a+(r.skipped||0),0);
      const parts=[];
      if(folders) parts.push(folders+" folder"+(folders===1?"":"s"));
      if(files) parts.push(files+" file"+(files===1?"":"s"));
      let m=parts.length?parts.join(" and ")+" moved to _ToDelete.":"";
      if(skipped) m+=" "+skipped+" kept by the last-copy guard.";
      if(m) $("#msg").textContent=m;
    }
    if(qWasBusy && !busy){ clearInterval(qTimer); qTimer=null; await loadTrees(); }
    qWasBusy=busy;
  };
  await tick();
  qTimer=setInterval(tick, 1500);
}
loadTrees();
pollQuarantine();
load();"##;
    shell("duplicates", csrf, "Review duplicates", main, script)
}

pub fn drives_page(csrf: &str) -> String {
    let main = r##"
<div class="row" style="justify-content:space-between;align-items:flex-start;gap:16px;flex-wrap:wrap;margin-bottom:22px">
  <div><h1 class="page-h">Drives</h1><div class="page-sub" style="margin:0">Manage and monitor your catalogued storage volumes. Nothing here deletes files on your drives.</div></div>
  <div id="quarantine-alert"></div>
</div>
<div id="drives" class="drivegrid onecol"><span class="mut">Loading drives…</span></div>
<div class="sumgrid" id="summary" style="margin-top:20px"></div>
<div class="mut" id="msg" style="margin-top:12px;min-height:1.4em"></div>"##;
    let script = r##"
function bar(d){
  if(d.total_bytes==null) return `<div class="cap-line"><span class="mut">Capacity unknown (drive offline)</span></div>
    <div class="progressbar"><span style="width:0%"></span></div>`;
  const used=d.total_bytes-d.free_bytes, pct=Math.round(100*used/d.total_bytes);
  const col=d.has_errors?'var(--red)':d.connected?'var(--accent)':'var(--gray)';
  return `<div class="cap-line"><span>${fmtSize(used)} of ${fmtSize(d.total_bytes)} used</span><span class="pct">${pct}% full</span></div>
    <div class="progressbar"><span style="width:${pct}%;background:${col}"></span></div>`;
}
function statusLine(d){
  if(d.has_errors){
    const bits=[];
    if(d.absent) bits.push(`${d.absent} not catalogued`);
    if(d.unverified) bits.push(`${d.unverified} unverified`);
    if(d.unreadable_dirs) bits.push(`${d.unreadable_dirs} unreadable folder${d.unreadable_dirs>1?'s':''}`);
    return `<span class="sdot" style="background:var(--red)"></span><span style="color:var(--red)">${bits.join(' · ')}</span>`;
  }
  if(d.connected) return `<span class="sdot" style="background:var(--green)"></span>Active · connected`;
  return `<span class="sdot" style="background:var(--gray)"></span>Offline`;
}
async function load(){
  const [drives,st]=await Promise.all([apiGet("/api/drives"),apiGet("/api/stats").catch(()=>null)]);
  $("#drives").innerHTML = drives.length? drives.map(d=>`<div class="card drivecard${d.has_errors?' err':''}" data-vid="${esc(d.volume_id)}" data-path="${esc(d.mount_path||'')}">
    <div class="dhead">
      <div class="card-ico" style="background:color-mix(in srgb,${driveColor(d.volume_id)} 14%,transparent);color:${driveColor(d.volume_id)}"><span class="material-symbols-outlined">hard_drive</span></div>
      <div style="flex:1;min-width:0">
        <div class="row" style="gap:8px"><h3 style="margin:0">${esc(d.display_name||d.label)}</h3><span class="lastscan">Last scan: ${fmtDate(d.last_seen_at)}</span></div>
        <div class="status-line">${statusLine(d)} · ${d.active_files.toLocaleString()} files</div>
        ${d.description?`<div class="mut" style="font-size:12px;margin-top:4px">${esc(d.description)}</div>`:""}
      </div>
    </div>
    ${bar(d)}
    <div class="actions">
      <button class="btn rescan" ${d.connected?'':'disabled'}><span class="material-symbols-outlined">${d.has_errors?'build':'refresh'}</span>${d.has_errors?'Repair':'Update catalog'}</button>
      <button class="btn edit"><span class="material-symbols-outlined">edit</span>Edit</button>
      <button class="btn btn-danger iconbtn forget" title="Forget this drive"><span class="material-symbols-outlined">delete</span></button>
    </div>
    <details class="completeness" data-vid="${esc(d.volume_id)}">
      <summary>Completeness</summary>
      <div class="cbody mut">loading…</div>
    </details>
    <div class="edit-form" style="display:none;margin-top:14px;padding-top:14px;border-top:1px solid var(--line)">
      <input class="ef-name" type="text" placeholder="Custom name (blank = detected)" style="width:100%;margin-bottom:8px" value="${esc(d.display_name||'')}">
      <input class="ef-desc" type="text" placeholder="Short description" style="width:100%;margin-bottom:10px" value="${esc(d.description||'')}">
      <div class="row"><button class="btn btn-primary ef-save">Save</button><button class="btn ef-cancel">Cancel</button></div>
    </div></div>`).join("")
    : '<div class="mut" style="grid-column:1 / -1">No drives catalogued yet. Scan one from the Scan page.</div>';

  // "Purge all" acts on files already in _ToDelete (quarantined_bytes) — not the forward-looking
  // reclaimable-from-duplicates figure, which drops to 0 the moment you quarantine a copy.
  const totQuar=drives.reduce((a,d)=>a+(d.quarantined_bytes||0),0);
  const withQ=drives.filter(d=>(d.quarantined_bytes||0)>0).length;
  $("#quarantine-alert").innerHTML = totQuar>0 ? `<div class="q-alert">
    <div class="q-ico"><span class="material-symbols-outlined">delete_sweep</span></div>
    <div class="qtxt"><strong>Quarantined files</strong><span>${fmtSize(totQuar)} across ${withQ} drive${withQ===1?'':'s'}</span></div>
    <button class="btn btn-danger" id="purge-all" style="margin-left:6px">Purge all</button></div>` : "";
  const q=$("#purge-all"); if(q) q.onclick=purgeAll;

  const totReclaim=drives.reduce((a,d)=>a+(d.reclaimable_bytes||0),0);
  const totBytes=drives.reduce((a,d)=>a+(d.active_bytes||0),0);
  const totFiles=drives.reduce((a,d)=>a+(d.active_files||0),0);
  const groups=st?st.duplicate_groups:0;
  $("#summary").innerHTML = drives.length ? `
    <div class="card sumcard"><div class="k">Total catalogued</div><div class="v">${fmtSize(totBytes)}</div><div class="s">Across ${drives.length} volume${drives.length===1?'':'s'}</div></div>
    <div class="card sumcard"><div class="k">Files indexed</div><div class="v">${totFiles.toLocaleString()}</div><div class="s">Active entries in the catalog</div></div>
    <div class="card sumcard"><div class="k">Reclaimable</div><div class="v accent">${fmtSize(totReclaim)}</div><div class="s">${groups} duplicate group${groups===1?'':'s'} to review${totQuar>0?` · ${fmtSize(totQuar)} in quarantine`:''}</div></div>` : "";

  for(const c of document.querySelectorAll(".drivecard[data-vid]")){
    c.querySelector(".forget").onclick=async()=>{
      const vid=c.dataset.vid, label=c.querySelector("h3").textContent;
      if(!window.confirm(`Forget "${label}"? This removes it from the catalog only — files on the drive are NOT deleted. You can rescan to re-add it.`))return;
      try{ const r=await apiPost("/api/forget-drive",{volume_id:vid}); $("#msg").textContent=`Forgot ${label} (${r.removed_files} entries removed).`; load(); }
      catch(e){ $("#msg").textContent="Error: "+e; }
    };
    c.querySelector(".rescan").onclick=async()=>{
      const path=c.dataset.path; if(!path){ $("#msg").textContent="Drive not connected."; return; }
      try{ await apiPost("/api/scan",{path,force:false}); $("#msg").textContent="Rescan queued for "+path+". Watch progress on the Scan page."; }
      catch(e){ $("#msg").textContent="Error: "+e; }
    };
    const form=c.querySelector(".edit-form");
    c.querySelector(".edit").onclick=()=>{ form.style.display = form.style.display==="none"?"block":"none"; };
    c.querySelector(".ef-cancel").onclick=()=>{ form.style.display="none"; };
    c.querySelector(".ef-save").onclick=async()=>{
      try{
        await apiPost("/api/rename-drive",{volume_id:c.dataset.vid,
          name:c.querySelector(".ef-name").value, description:c.querySelector(".ef-desc").value});
        $("#msg").textContent="Drive updated."; load();
      }catch(e){ $("#msg").textContent="Error: "+e; }
    };
  }
}
async function purgeAll(){
  if(!window.confirm("Permanently delete every drive's _ToDelete quarantine? This is the only real delete and cannot be undone."))return;
  try{ const r=await apiPost("/api/purge-all",{});
    let m=`Purged ${r.purged_volumes} volume(s), reclaimed ${fmtSize(r.bytes_reclaimed)}.`;
    if(r.skipped_unmounted.length)m+=" Skipped (offline): "+r.skipped_unmounted.join(", ")+".";
    if(r.errors.length)m+=" Errors: "+r.errors.join("; ");
    $("#msg").textContent=m; load(); }
  catch(e){ $("#msg").textContent="Error: "+e; }
}
// Fetched only when opened: a drive with thousands of failures should not cost anything on
// page load, and the counts on the card already answer the common question.
document.addEventListener('toggle', async e=>{
  const el=e.target;
  if(!el.matches('details.completeness') || !el.open || el.dataset.loaded) return;
  el.dataset.loaded='1';
  const body=el.querySelector('.cbody');
  try{
    const r=await fetch(`/api/volumes/${encodeURIComponent(el.dataset.vid)}/errors`);
    const d=await r.json();
    if(!d.rows.length){ body.innerHTML='<span class="cnote">Complete — every file was catalogued.</span>'; return; }
    // The last segment is what identifies the file; the rest is context. Emphasising it keeps the
    // useful part readable when the ellipsis eats the middle of a deeply nested archive path.
    const leaf=p=>{ const s=String(p).split(/[\/›]/); return s[s.length-1].trim()||p; };
    body.innerHTML=d.rows.map(x=>{
      const reason=x.reason||'recorded before classification';
      const head=esc(x.path.slice(0,x.path.length-leaf(x.path).length));
      return `<div class="erow" title="Click to show the full path and reason">
        <span class="tag">${esc(x.bucket==='unreadable_dir'?'folder':x.bucket)}</span>
        <span class="epath" title="${esc(x.path)}">${head}<b>${esc(leaf(x.path))}</b></span>
        <span class="ereason" title="${esc(reason)}">${esc(reason)}</span>
      </div>`;
    }).join('')
      + (d.rows.length>=200?'<span class="cnote">showing the first 200</span>':'');
  }catch(err){ body.innerHTML='<span class="cnote">Could not load the error list.</span>'; }
}, true);
// Rows are display:contents, so there is no row box to click — toggle via the cell's parent.
document.addEventListener('click', e=>{
  const cell=e.target.closest('.cbody > .erow > span');
  if(cell) cell.parentElement.classList.toggle('open');
});
load().catch(e=>{$("#drives").textContent="Error: "+e;});"##;
    shell("drives", csrf, "Drives", main, script)
}

pub fn scan_page(csrf: &str) -> String {
    let main = r##"
<h1 class="page-h">Scan a Drive</h1>
<div class="page-sub">Connect a drive or select a folder to catalog your files and identify duplicates. Nothing is modified except a tiny hidden marker used to recognise the drive next time.</div>
<div class="sec-label" style="margin-top:6px">Detected drives</div>
<div id="drives" class="drivegrid"><span class="mut">Looking for connected drives…</span></div>
<div class="sec-label">Or choose a folder</div>
<div class="folderbar">
  <label class="folderin"><span class="material-symbols-outlined" style="color:var(--mut);font-size:20px">folder_open</span>
    <input id="path" type="text" placeholder="Type or browse to a folder to scan…"></label>
  <button class="btn" id="browse"><span class="material-symbols-outlined">drive_folder_upload</span>Browse…</button>
  <label class="row" style="font-size:13px;color:var(--mut);gap:10px">
    <span class="switch"><input id="force" type="checkbox"><span class="sl"></span></span> Force full rescan
  </label>
</div>
<div style="text-align:center;margin:22px 0 4px">
  <button class="btn btn-primary" id="scan" style="padding:11px 26px;font-size:14px"><span class="material-symbols-outlined">play_arrow</span>Scan Selected</button>
</div>
<div class="mut" id="running" style="text-align:center;min-height:1.2em;margin-bottom:16px"></div>
<div class="card" id="status-card">
  <div class="row" style="justify-content:space-between;align-items:flex-start">
    <div><h3 style="margin:0" id="status-title">Live status</h3><div class="mut" id="status-sub" style="font-size:12.5px;margin-top:2px">No scan running.</div></div>
    <button class="btn" id="stopscan" hidden>Stop scan</button>
  </div>
  <div class="progressbar" id="pbar" style="margin:16px 0 0;display:none"><span style="width:40%"></span></div>
  <div class="statcols" id="tiles" style="display:none">
    <div class="statcol"><div class="k">Hashed</div><div class="v mono accent" id="t-hashed">0</div></div>
    <div class="statcol"><div class="k">Unchanged</div><div class="v mono" id="t-skip">0</div></div>
    <div class="statcol"><div class="k">Errors</div><div class="v mono" id="t-err">0</div></div>
    <div class="statcol"><div class="k">Archive entries</div><div class="v mono" id="t-arch">0</div></div>
  </div>
  <div class="mut" id="queued" style="margin-top:10px;font-size:12.5px"></div>
</div>
<div class="card">
  <h3 style="margin:0 0 4px">Recent scans</h3>
  <div id="recent" class="mut">None yet.</div>
</div>
<div class="card">
  <h3 style="margin:0 0 4px">Scan timings</h3>
  <div class="mut" style="font-size:12px;margin-bottom:10px">Where past scans spent their time. Kept across restarts, unlike the list above.</div>
  <div id="runs"><span class="mut">Loading…</span></div>
</div>
<div class="card" id="pendingfmt" style="margin-top:16px;display:none">
  <b>Unrecognised zip-format files found</b>
  <div class="mut" style="margin:6px 0 10px">
    These are zip files with an extension the scanner does not recognise. They were catalogued whole
    and <b>not</b> opened. Approve one to catalogue what is inside on the next scan.
  </div>
  <div id="pendingfmtbody"></div>
  <div class="mut" id="pendingfmtmsg" style="min-height:1.3em;margin-top:8px"></div>
</div>
<details class="card" id="limits" style="margin-top:16px">
  <summary style="cursor:pointer">Archive limits</summary>
  <div class="mut" style="margin:8px 0 12px">
    Applies to the <b>next</b> scan — a run already in progress keeps the limits it started with.
  </div>
  <div id="limitsbody" class="mut">loading…</div>
  <div id="limitsmsg" class="mut" style="min-height:1.3em;margin-top:8px"></div>
</details>"##;
    let script = r##"
function baseName(p){ const s=String(p).replace(/[\\/]+$/,""); const m=s.split(/[\\/]/); return m[m.length-1]||s; }
async function loadDrives(){
  try{
    const ds=await apiGet("/api/detected-drives");
    if(!ds.length){ $("#drives").innerHTML='<span class="mut">No drives detected. Choose a folder below.</span>'; return; }
    $("#drives").innerHTML=ds.map(d=>{
      const badge=d.catalogued?'<span class="tag">Catalogued · rescan</span>':'<span class="tag tone-blue" style="font-family:var(--font-ui)">New</span>';
      let cap="";
      if(d.total_bytes!=null){ const used=d.total_bytes-(d.free_bytes||0); const pct=Math.round(100*used/d.total_bytes);
        cap=`<div class="progressbar" style="margin-top:9px"><span style="width:${pct}%"></span></div>
        <div class="dcap"><span>${fmtSize(d.free_bytes)} free</span><span>${fmtSize(d.total_bytes)} total</span></div>`; }
      return `<div class="card dcard" data-path="${esc(d.mount_path)}">
        <div class="dtop"><div class="card-ico"><span class="material-symbols-outlined">hard_drive</span></div>
          <div class="txt"><div class="row" style="justify-content:space-between;gap:8px"><span class="dname">${esc(d.volume_label||baseName(d.mount_path))}</span>${badge}</div>
          <div class="dpath">${esc(d.mount_path)}</div></div></div>
        ${cap}</div>`;
    }).join("");
    for(const el of document.querySelectorAll("#drives .dcard")) el.addEventListener("click",()=>{
      $("#path").value=el.dataset.path;
      for(const o of document.querySelectorAll("#drives .dcard")) o.classList.toggle("sel",o===el);
    });
  }catch(e){ $("#drives").innerHTML='<span class="mut">Could not list drives: '+esc(String(e))+'</span>'; }
}
$("#browse").addEventListener("click",async()=>{
  try{
    const j=await apiPost("/api/pick-folder");
    if(j.path) $("#path").value=j.path;
  }catch(e){ $("#running").textContent="Folder picker error: "+e; }
});
$("#scan").addEventListener("click",async()=>{
  const path=$("#path").value.trim(); if(!path){ $("#running").textContent="Select a drive or choose a folder first."; return; }
  const force=$("#force").checked;
  try{
    await apiPost("/api/scan",{path,force});
    $("#running").textContent="";
    poll();
  }catch(e){ $("#running").innerHTML='<span style="color:var(--red)">Scan error: '+esc(String(e))+'</span>'; }
});
$("#stopscan").addEventListener("click", async ()=>{
  $("#stopscan").disabled = true;
  try {
    await apiPost("/api/scan/stop", {});
    $("#running").textContent = "Stopping after the current file… nothing will be marked missing.";
  } catch(e) { $("#running").innerHTML = '<span style="color:var(--red)">Could not stop: '+esc(String(e))+'</span>'; }
  $("#stopscan").disabled = false;
});
// "1h 42m" / "3m 20s" / "45s" -- mirrors fmt_duration in scan_control.rs so the CLI and the web UI
// read the same eta the same way.
function fmtEta(s){ return s>=3600 ? `${Math.floor(s/3600)}h ${String(Math.floor(s%3600/60)).padStart(2,"0")}m`
  : s>=60 ? `${Math.floor(s/60)}m ${String(s%60).padStart(2,"0")}s` : `${s}s`; }
function setTiles(on){ $("#tiles").style.display=on?"grid":"none"; const p=$("#pbar"); p.style.display=on?"block":"none"; p.classList.toggle("run",on); }
async function poll(){
  try{
    const s=await apiGet("/api/scan/status");
    if(s.running){ const r=s.running;
      $("#status-title").textContent="Active scan: "+baseName(r.path);
      // Percentage and ETA are both absent (not zero) until the counting pass has reported totals
      // and the rate estimator has enough samples -- never show a number we can't back up.
      const pct = (r.total_bytes && r.total_bytes > 0)
        ? Math.min(100, Math.round(r.done_bytes * 100 / r.total_bytes)) : null;
      const eta = r.eta_seconds != null ? fmtEta(r.eta_seconds) : null;
      $("#status-sub").textContent =
        (pct != null ? pct + "% · " : "") +
        `${r.hashed} hashed · ${r.skipped} unchanged` +
        (eta != null ? ` · ETA ${eta}` : "");
      $("#t-hashed").textContent=r.hashed.toLocaleString(); $("#t-skip").textContent=r.skipped.toLocaleString();
      $("#t-err").textContent=r.errors.toLocaleString(); $("#t-arch").textContent=r.archive_entries.toLocaleString();
      setTiles(true);
      $("#stopscan").hidden = false;
    } else { $("#status-title").textContent="Live status"; $("#status-sub").textContent="No scan running."; setTiles(false); $("#stopscan").hidden = true; }
    $("#queued").textContent = s.queued.length ? ("Queued: "+s.queued.join(", ")) : "";
    $("#recent").innerHTML = s.recent.length ? s.recent.map(r=>{
      const ok=!r.error_message;
      const ico=ok?'<div class="act-ico tone-green"><span class="material-symbols-outlined">check_circle</span></div>'
                  :'<div class="act-ico tone-red"><span class="material-symbols-outlined">error</span></div>';
      const sub=ok?`${r.hashed} hashed · ${r.skipped} unchanged · ${r.errors} errors · ${r.archive_entries} archive entries`
                  :`<span style="color:var(--red)">${esc(r.error_message)}</span>`;
      return `<div class="recentrow">${ico}
        <div style="flex:1;min-width:0"><div class="act-title mono" style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(r.path)}</div>
        <div class="mut" style="font-size:12px;margin-top:2px">${sub}</div></div></div>`;
    }).join("") : '<div class="mut" style="padding:6px 0">No scans yet.</div>';
    if(s.running || s.queued.length) setTimeout(poll, 1500);
  }catch(e){ /* stop polling on error */ }
}
function runRow(r){
  const m=r.metrics;
  // Percent of WALL, matching the CLI. Dividing by the sum of phases would force them to total
  // 100% and hide exactly the overlap #23 is meant to create.
  const pct=v=>m.wall_ms?Math.round(v*100/m.wall_ms):0;
  const when=r.started_at?fmtDate(r.started_at):"";
  // "interrupted" is deliberately distinct from "cancelled": the user chose one and not the other,
  // and after a five-day scan that difference is what tells them whether to worry.
  const status=r.status==="running"?'<span class="mut">running…</span>'
    :r.status==="failed"?`<span style="color:var(--red)">failed</span>`
    :r.status==="interrupted"?`<span style="color:var(--red)" title="The scan ended without finishing — power loss, the drive being removed, or the process being killed. Nothing was marked missing; re-run to continue.">interrupted</span>`
    :esc(r.status);
  // Phase split as a single bar: the shape is the point, not the exact numbers.
  const bar=[["walk",m.walk_ms],["skip",m.skip_check_ms],["hash",m.hash_ms],
             ["db",m.db_write_ms],["arch",m.archive_ms]]
    .filter(([,v])=>v>0)
    .map(([k,v])=>`<span title="${k} ${v} ms">${k} ${pct(v)}%</span>`).join(" · ");
  return `<div class="dl"><span class="k">${esc(when)} — ${esc(r.root_path)}</span>
    <span class="v">${status}</span></div>
    <div class="mut" style="font-size:12px;padding:0 0 10px">
      ${r.hashed} hashed · ${r.skipped} unchanged · ${r.errors} errors ·
      ${fmtSize(m.bytes_hashed)} hashed in ${m.wall_ms} ms${bar?" · "+bar:""}
      ${r.error_message?"<br>"+esc(r.error_message):""}
    </div>`;
}
async function loadRuns(){
  try{
    const rs=await apiGet("/api/scan-runs?limit=10");
    $("#runs").innerHTML = rs.length ? rs.map(runRow).join("")
      : '<span class="mut">No scans recorded yet.</span>';
  }catch(e){ $("#runs").innerHTML='<span class="mut">Could not load scan history.</span>'; }
}
loadDrives(); poll(); loadRuns(); loadPendingFormats();
function humanBytes(n){
  const v=Number(n);
  if(!Number.isFinite(v)) return '';
  const u=['B','KB','MB','GB','TB'];
  let i=0, x=v;
  while(x>=1024 && i<u.length-1){ x/=1024; i++; }
  return (i===0? x : x.toFixed(1))+' '+u[i];
}
async function loadPendingFormats(){
  let rows;
  try{ rows=await apiGet("/api/pending-formats"); }catch(e){ return; }
  const box=$("#pendingfmt");
  if(!rows.length){ box.style.display='none'; return; }
  box.style.display='';
  $("#pendingfmtbody").innerHTML=rows.map(r=>`<div class="erow">
      <span class="tag">${r.extension===''?'(no extension)':'.'+esc(r.extension)}</span>
      <span>${esc(String(r.count))} files</span>
      <span class="mut">${esc(humanBytes(r.total_bytes))}</span>
      <button class="btn pfmt" data-ext="${esc(r.extension)}" data-action="descend">Descend into these</button>
      <button class="btn pfmt" data-ext="${esc(r.extension)}" data-action="document">Treat as documents</button>
    </div>`).join('');
}
document.addEventListener('click', async e=>{
  const b=e.target.closest('button.pfmt');
  if(!b) return;
  const label=b.dataset.ext===''?'(no extension)':'.'+b.dataset.ext;
  try{
    await apiPost("/api/pending-formats/resolve", {extension:b.dataset.ext, action:b.dataset.action});
    $("#pendingfmtmsg").textContent =
      b.dataset.action==='descend'
        ? `${label} will be opened on the next scan.`
        : `${label} will be catalogued whole.`;
    // A resolve moves the extension onto one of the settings lists -- refresh the chips too, or
    // the pending row vanishes with nothing visibly taking its place, which reads as "did
    // nothing" (or worse, "threw the file away") rather than "moved to the other list".
    await loadPendingFormats();
    await refreshLimits();
  }catch(err){ $("#pendingfmtmsg").textContent="Failed: "+err.message; }
});
const LIMITS=[
  ["archive_ratio_cap","Ratio cap","Refuses an entry whose declared uncompressed/compressed ratio is higher. Guards time, not memory: real files reach the hundreds, a zip bomb reaches the millions.",false],
  ["archive_entry_max_bytes","Largest file in an archive (bytes)","Leave empty for unlimited. Files inside archives are streamed, so this bounds how long one entry may take, not how much memory it uses.",true],
  ["archive_buffer_max_bytes","Nested archive buffer (bytes)","Real memory: a zip inside a zip is held in RAM so it can be hashed and re-opened.",true],
  ["archive_total_buffer_bytes","Total buffer budget (bytes)","Real memory: the ceiling on all nested-archive buffers alive at once.",true],
  ["max_archive_depth","Maximum nesting depth","",false],
];
const EXTLISTS=[
  ["archive_deny_extensions","Treated as documents (never opened as archives)"],
  ["archive_allow_extensions","Always descended into (even if otherwise unrecognised)"],
];
let LASTSETTINGS=null;
function extGroupHtml(key,title,list){
  const chips=list.length
    ? `<div class="chiprow">${list.map(ex=>`<span class="chip">${ex===''?'(no extension)':'.'+esc(ex)}
        <button class="chiprm" data-list="${esc(key)}" data-ext="${esc(ex)}" title="Remove">×</button></span>`).join('')}</div>`
    : `<div class="mut" style="font-size:12px">None.</div>`;
  return `<div style="margin-bottom:12px">
    <label>${esc(title)}</label>
    ${chips}
  </div>`;
}
async function loadLimits(){
  const d=await apiGet("/api/settings");
  LASTSETTINGS=d;
  $("#limitsbody").innerHTML=LIMITS.map(([k,label,help,isBytes])=>`
    <div style="margin-bottom:10px">
      <label for="lim_${esc(k)}">${esc(label)}</label>
      <input id="lim_${esc(k)}" name="${esc(k)}" value="${d[k]===null||d[k]===undefined?'':esc(String(d[k]))}" style="width:100%">
      ${isBytes?`<span class="mut" id="hint_${esc(k)}" style="font-size:12px"></span>`:''}
      ${help?`<div class="mut" style="font-size:12px">${esc(help)}</div>`:''}
    </div>`).join('')
    + EXTLISTS.map(([key,title])=>extGroupHtml(key,title,d[key]||[])).join('')
    + `<button class="btn" id="savelimits">Save</button>`;
  for(const [k,,,isBytes] of LIMITS){
    if(!isBytes) continue;
    const inp=$("#lim_"+k), hint=$("#hint_"+k);
    const update=()=>{ hint.textContent = inp.value.trim()===''?'':'= '+humanBytes(inp.value); };
    update();
    inp.addEventListener('input', update);
  }
}
document.addEventListener('click', async e=>{
  const b=e.target.closest('button.chiprm');
  if(!b) return;
  const key=b.dataset.list, ext=b.dataset.ext;
  const cur=(LASTSETTINGS && LASTSETTINGS[key]) || [];
  const next=cur.filter(x=>x!==ext);
  try{
    await apiPost("/api/settings", {[key]: next});
    $("#limitsmsg").textContent="Removed.";
    await loadLimits();
  }catch(err){ $("#limitsmsg").textContent="Failed: "+err.message; }
});
// Shared by the details-toggle handler below and by anything that changes a setting the section
// displays (resolving a pending format) so the two never drift out of sync. Sets the "loaded" flag
// only once the load succeeds, so a failure leaves the section retryable on the next collapse/
// expand -- and so a caller who refreshes while the section has never been opened correctly marks
// it as already fresh, instead of the next open silently re-fetching (harmless) or, if the flag
// were set unconditionally, silently skipping a real reload after a failed one (not harmless).
function refreshLimits(){
  return loadLimits().then(()=>{ $("#limits").dataset.loaded='1'; })
    .catch(()=>{ $("#limitsbody").textContent='Could not load the limits.'; });
}
document.addEventListener('toggle', e=>{
  if(e.target.id==='limits' && e.target.open && !e.target.dataset.loaded){
    refreshLimits();
  }
}, true);
document.addEventListener('click', async e=>{
  if(e.target.id!=='savelimits') return;
  const body={};
  for(const [k,label] of LIMITS){
    const raw=$("#lim_"+k).value.trim();
    // Empty means "unlimited" for the leaf ceiling and "leave unset" for everything else.
    if(raw==='') { if(k==='archive_entry_max_bytes') body[k]=null; continue; }
    const n=Number(raw);
    if(!Number.isFinite(n)||n<0){ $("#limitsmsg").textContent=`${label} must be a number.`; return; }
    body[k]=n;
  }
  // apiPost (defined in the shared shell, web_ui.rs:370) sends the x-cleanup-token header and
  // THROWS on a non-2xx, returning parsed JSON otherwise -- so the refusal arrives as an
  // exception, not as a falsy `ok`.
  try{
    await apiPost("/api/settings", body);
    $("#limitsmsg").textContent="Saved — applies to the next scan.";
    await loadPendingFormats();
  }catch(err){
    $("#limitsmsg").textContent="Refused: "+err.message;
  }
});"##;
    shell("scan", csrf, "Scan a drive", main, script)
}

/// Client-side-only REPL: parses a typed line into one existing `/api/*` call (the same
/// endpoints the buttons use, via SHARED_JS `apiGet`/`apiPost`) and prints the JSON response as
/// scrollback text. No new backend, no subprocess, no shell execution — unrecognized input
/// prints a usage hint and makes no request.
pub fn console_page(csrf: &str) -> String {
    let main = r##"
<h1 class="page-h">Console</h1>
<div class="page-sub">Runs this app's own commands only — the same safe actions as the buttons. Type <span class="mono">help</span>.</div>
<div id="out" class="console-out" aria-live="polite"></div>
<div class="console-inbar"><span class="prompt">$</span>
  <input id="cmd" class="console-in" placeholder="status  ·  search thesis  ·  scan D:\ --force" autofocus></div>"##;
    let script = r##"
const out=$("#out");
function print(s,cls){ const d=document.createElement("div"); if(cls)d.className=cls; d.textContent=s; out.appendChild(d); out.scrollTop=out.scrollHeight; }
function printJSON(o){ print(JSON.stringify(o,null,2)); }
const HELP=`Commands:
  status                         catalog summary
  duplicates                     list duplicate groups
  search <query> [--category c] [--status s]
  scan <path> [--force]          queue a scan
  quarantine <id> [id ...]       quarantine file ids
  repack <id>                    remove an in-archive duplicate
  forget <volumeId>              remove a drive from the catalog
  purge --all                    purge every mounted quarantine
  drives                         list drives
  help, clear`;
// naive shell-ish tokenizer: splits on whitespace, honours "double quotes".
function tokenize(line){ const m=line.match(/"[^"]*"|\S+/g)||[]; return m.map(t=>t.replace(/^"|"$/g,"")); }
function flag(toks,name){ const i=toks.indexOf("--"+name); if(i<0)return null; const v=toks[i+1]; toks.splice(i, v&&!v.startsWith("--")?2:1); return v||true; }
async function exec(line){
  print("$ "+line);
  const toks=tokenize(line); const cmd=(toks.shift()||"").toLowerCase();
  try{
    if(cmd==="help"||cmd===""){ print(HELP); return; }
    if(cmd==="clear"){ out.innerHTML=""; return; }
    if(cmd==="status"){ printJSON(await apiGet("/api/stats")); return; }
    if(cmd==="drives"){ printJSON(await apiGet("/api/drives")); return; }
    if(cmd==="duplicates"){ printJSON(await apiGet("/api/duplicates?limit=20")); return; }
    if(cmd==="search"){ const cat=flag(toks,"category"), st=flag(toks,"status");
      const p=new URLSearchParams(); if(toks.length)p.set("q",toks.join(" ")); if(cat)p.set("category",cat); if(st)p.set("status",st);
      printJSON(await apiGet("/api/search?"+p.toString())); return; }
    if(cmd==="scan"){ const force=!!flag(toks,"force"); const path=toks.join(" ");
      if(!path){ print("usage: scan <path> [--force]","mut"); return; }
      printJSON(await apiPost("/api/scan",{path,force})); return; }
    if(cmd==="quarantine"){ const ids=toks.map(Number).filter(n=>!isNaN(n));
      if(!ids.length){ print("usage: quarantine <id> [id ...]","mut"); return; }
      const r=await apiPost("/api/quarantine",{quarantine_ids:ids});
      printJSON(r); print("Queued — the worker runs these in order; watch /api/quarantine/status."); return; }
    if(cmd==="repack"){ const id=Number(toks[0]); if(isNaN(id)){ print("usage: repack <id>","mut"); return; }
      printJSON(await apiPost("/api/repack",{entry_id:id})); return; }
    if(cmd==="forget"){ if(!toks[0]){ print("usage: forget <volumeId>","mut"); return; }
      printJSON(await apiPost("/api/forget-drive",{volume_id:toks[0]})); return; }
    if(cmd==="purge"){ if(flag(toks,"all")){ printJSON(await apiPost("/api/purge-all",{})); return; }
      print("only 'purge --all' is supported from the console; use the Drives page for a single drive.","mut"); return; }
    print("unknown command: "+cmd+" (try 'help')","mut");
  }catch(e){ print("error: "+e,"mut"); }
}
$("#cmd").addEventListener("keydown",e=>{ if(e.key==="Enter"){ const v=e.target.value; e.target.value=""; if(v.trim())exec(v.trim()); }});
print(HELP);"##;
    shell("console", csrf, "Console", main, script)
}

#[cfg(test)]
mod tests {
    /// Parse every page's JavaScript and fail if any of it is not valid.
    ///
    /// This exact defect shipped. A line break written into the Rust raw string where the
    /// two-character escape was meant left `confirm("...` unterminated — a JavaScript syntax error,
    /// so the whole page script failed to parse and every list on the page rendered nothing, the
    /// duplicates list included, with no error the user could see.
    ///
    /// Nothing else in the build looks at this JavaScript. A hand-rolled scanner was tried first and
    /// produced false positives on regex literals (`/[&<>"']/g`), because telling a regex from a
    /// division needs real parsing — so this shells out to a real parser rather than approximating
    /// one badly.
    ///
    /// Skips, loudly, when `node` is unavailable: a missing tool must not look like a pass.
    fn js_of(page: &str) -> String {
        page.split("<script>")
            .skip(1)
            .filter_map(|s| s.split("</script>").next())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_page_emits_parseable_javascript() {
        let probe = std::process::Command::new("node").arg("--version").output();
        if probe.is_err() {
            eprintln!("SKIPPED: `node` not on PATH, cannot parse-check page JavaScript");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        for (name, html) in [
            ("overview", super::overview_page("t")),
            ("browse", super::browse_page("t")),
            ("review", super::review_page("t")),
            ("drives", super::drives_page("t")),
            ("scan", super::scan_page("t")),
        ] {
            let js = js_of(&html);
            assert!(!js.is_empty(), "{name}: no <script> found to check");
            let path = dir.path().join(format!("{name}.js"));
            std::fs::write(&path, &js).unwrap();

            let out = std::process::Command::new("node")
                .arg("--check")
                .arg(&path)
                .output()
                .expect("node --check should run");
            assert!(
                out.status.success(),
                "{name} page emits invalid JavaScript, which silently blanks the whole page:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
