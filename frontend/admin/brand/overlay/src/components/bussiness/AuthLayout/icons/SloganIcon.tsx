import type React from 'react';

/**
 * 品牌标识「锐风 RushWind」（RushWind 框架仓 assets/logo，R 字单标）：
 * R 字主标（斜腿收细甩出风痕）+ 双层对旋虚线环流 + 轨道光点 + 四向漂移风痕。
 * 色板同源 RushWind 品牌：Wind Teal #2DD4BF → Wind Sky #38BDF8 → Wind Indigo #818CF8，
 * 渐变方向固定左下 → 右上（与「风掠过」方向一致）。
 * 动效尊重 prefers-reduced-motion；浮动动效由父级样式提供。
 * 注：SVG 内嵌 <style> 为文档级作用域，keyframes/类名均带 rw- 前缀防碰撞。
 */
const ANIMATION_CSS = `
.rw-ring1,.rw-ring2,.rw-orbit,.rw-orbit2{transform-box:view-box;transform-origin:280px 250px}
.rw-ring1{animation:rw-spin 48s linear infinite}
.rw-ring2{animation:rw-spin 70s linear infinite reverse}
.rw-orbit{animation:rw-spin 36s linear infinite}
.rw-orbit2{animation:rw-spin 52s linear infinite reverse}
.rw-streak{animation:rw-drift 9s ease-in-out infinite alternate}
.rw-s2{animation-duration:12s;animation-delay:-4s}
.rw-s3{animation-duration:10s;animation-delay:-2s}
.rw-s4{animation-duration:13s;animation-delay:-6s}
@keyframes rw-spin{to{transform:rotate(360deg)}}
@keyframes rw-drift{from{transform:translateX(-16px)}to{transform:translateX(16px)}}
@media (prefers-reduced-motion:reduce){
.rw-ring1,.rw-ring2,.rw-orbit,.rw-orbit2,.rw-streak{animation:none}
}
`;

const SloganIcon: React.FC<React.SVGProps<SVGSVGElement>> = (props) => (
  <svg
    viewBox="0 0 560 560"
    xmlns="http://www.w3.org/2000/svg"
    role="img"
    aria-label="RushWind Admin"
    width="100%"
    height="100%"
    {...props}
  >
    <style>{ANIMATION_CSS}</style>
    <defs>
      <linearGradient
        id="rw-slogan-grad"
        gradientUnits="userSpaceOnUse"
        x1="96"
        y1="410"
        x2="470"
        y2="110"
      >
        <stop offset="0" stopColor="#2DD4BF" />
        <stop offset="0.5" stopColor="#38BDF8" />
        <stop offset="1" stopColor="#818CF8" />
      </linearGradient>
      <radialGradient id="rw-slogan-glow" cx="0.5" cy="0.5" r="0.5">
        <stop offset="0" stopColor="#38BDF8" stopOpacity="0.12" />
        <stop offset="1" stopColor="#38BDF8" stopOpacity="0" />
      </radialGradient>
    </defs>
    <circle cx="280" cy="250" r="252" fill="url(#rw-slogan-glow)" />
    <circle
      className="rw-ring2"
      cx="280"
      cy="250"
      r="246"
      fill="none"
      stroke="#38BDF8"
      strokeOpacity="0.07"
      strokeWidth="1.5"
      strokeDasharray="2 16"
      strokeLinecap="round"
    />
    <circle
      className="rw-ring1"
      cx="280"
      cy="250"
      r="212"
      fill="none"
      stroke="#2DD4BF"
      strokeOpacity="0.14"
      strokeWidth="1.5"
      strokeDasharray="3 13"
      strokeLinecap="round"
    />
    <g className="rw-orbit">
      <circle cx="280" cy="95" r="4.5" fill="#2DD4BF" opacity="0.55" />
      <circle cx="280" cy="405" r="3" fill="#818CF8" opacity="0.45" />
    </g>
    <g className="rw-orbit2">
      <circle cx="435" cy="250" r="3.5" fill="#38BDF8" opacity="0.4" />
    </g>
    <g
      className="rw-streak"
      fill="none"
      stroke="#2DD4BF"
      strokeOpacity="0.12"
      strokeWidth="10"
      strokeLinecap="round"
    >
      <path d="M52 190 H140 A20 20 0 0 0 120 156" />
    </g>
    <g
      className="rw-streak rw-s2"
      fill="none"
      stroke="#38BDF8"
      strokeOpacity="0.1"
      strokeWidth="10"
      strokeLinecap="round"
    >
      <path d="M400 126 H494 A22 22 0 0 0 474 88" />
    </g>
    <g
      className="rw-streak rw-s3"
      fill="none"
      stroke="#2DD4BF"
      strokeOpacity="0.12"
      strokeWidth="10"
      strokeLinecap="round"
    >
      <path d="M368 440 H480 A24 24 0 0 0 460 402" />
    </g>
    <g
      className="rw-streak rw-s4"
      fill="none"
      stroke="#38BDF8"
      strokeOpacity="0.09"
      strokeWidth="10"
      strokeLinecap="round"
    >
      <path d="M84 465 H161 A18 18 0 0 0 143 435" />
    </g>
    {/* R 字主标（几何取自 rushwind-icon，整体静态；缓旋只留给环流与光点） */}
    <g transform="translate(280,250) scale(0.95) translate(-256,-257)">
      <g fill="none" stroke="url(#rw-slogan-grad)" strokeLinecap="round">
        <path d="M 100 212 H 136" strokeWidth="14" />
        <path d="M 92 260 H 128" strokeWidth="14" />
        <path d="M 176 172 V 356" strokeWidth="48" />
        <path d="M 176 172 H 256 A 56 56 0 0 1 256 284 H 176" strokeWidth="48" />
        <path
          d="M 190 265 C 300 346 368 360 446 322 C 402 360 296 378 162 303 Z"
          fill="url(#rw-slogan-grad)"
          stroke="none"
        />
        <path d="M 456 330 L 476 324" strokeWidth="8" />
      </g>
    </g>
  </svg>
);

export default SloganIcon;
