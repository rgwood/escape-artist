import { h, render } from "/preact.js";
import htm from "/htm.js";
import {
  computePosition,
  flip,
  shift,
  offset,
  arrow
} from '/floating-ui.dom.js'

const html = htm.bind(h);

let url = new URL("/events", window.location.href);
// http => ws
// https => wss
url.protocol = url.protocol.replace("http", "ws");

let events = [];

function Event(props) {
  let dto = props.dto;
  const shared_classes = "w-fit border rounded-sm px-1 m-1 bg-slate-800"
  switch (dto.type) {
    case "Print":
      return html`<span>${dto.string}</span>`;
    case "GenericEscape": {
      let border = !!dto.tooltip ? "border-blue-400" : "border-slate-400";

      let showTooltip = (event) => {
        const tooltip = document.querySelector('#tooltip');
        tooltip.style.display = 'block';
        update(event.target);
      }
      return html`<span
        data-tooltip=${dto.tooltip}
        data-rawbytes=${dto.raw_bytes}
        onmouseenter=${showTooltip}
        onmouseleave=${hideTooltip}
        onfocus=${showTooltip}
        onblur=${hideTooltip}
        class="inline-block ${shared_classes} ${border} "
        >${dto.title}</span
      >`;
    }
    case "LineBreak": {
      return html`
      <div class="${shared_classes} border-slate-500 text-xs">
        ${dto.title}
      </div>`;
    }
    case "Disconnected": {
      return html`<div class="${shared_classes} border-red-500">
        Disconnected
      </div>`;
    }
    default:
      return html`<span class="inline-block ${shared_classes} border-slate-500">${dto.type}</span>`;
  }
}

let ws = new WebSocket(url.href);
ws.onmessage = async (ev) => {
  let deserialized = JSON.parse(ev.data);
  for (const event of deserialized) {
    events.push(event);
    // console.log(event);
  }
  renderAndScroll();
};

ws.onclose = (_) => {
  events.push({ type: "Disconnected" });
  renderAndScroll();
};

function renderAndScroll() {
  render(
    html`
    <div id="tooltip" class="hidden bg-slate-800 p-2 rounded-sm w-max absolute top-0 left-0" role="tooltip">
      <div class="flex flex-col items-center">
        <div id="description" class="font-sans font-semibold text-sm mb-1"/>
        <div id="rawbytes" class="w-max px-1 rounded-sm rounded-sm bg-slate-900"/>
      </div>
      <div id="arrow" class="absolute bg-slate-800 w-2 h-2 rotate-45"></div>
    </div>
    <a href="/help.html" class="bg-slate-800 hover:bg-cyan-900 p-0 font-bold rounded-sm border border-slate-400 m-2 w-8 text-center fixed top-0 right-0">❔</a>
    ${events.map((event) => html`<${Event} dto="${event}" />`)}
    `,
    document.body
  );
  window.scrollTo(0, document.body.scrollHeight);
}

function update(element) {
  const tooltip = document.querySelector('#tooltip');
  const arrowElement = document.querySelector('#arrow');

  const tooltipDescriptionElement = tooltip.querySelector("#description");
  if (!!element.dataset.tooltip) {
    tooltipDescriptionElement.style.display = 'block';
    tooltipDescriptionElement.innerHTML = element.dataset.tooltip;
  } else {
    tooltipDescriptionElement.style.display = 'none';
  }

  const tooltipRawBytes = tooltip.querySelector("#rawbytes");
  tooltipRawBytes.innerHTML = element.dataset.rawbytes;

  computePosition(element, tooltip, {
    placement: 'top',
    middleware: [
      offset(6),
      flip(),
      shift({ padding: 5 }),
      arrow({ element: arrowElement }),
    ],
  }).then(({ x, y, placement, middlewareData }) => {
    Object.assign(tooltip.style, {
      left: `${x}px`,
      top: `${y}px`,
    });

    const { x: arrowX, y: arrowY } = middlewareData.arrow;
    const staticSide = {
      top: 'bottom',
      right: 'left',
      bottom: 'top',
      left: 'right',
    }[placement.split('-')[0]];

    Object.assign(arrowElement.style, {
      left: arrowX != null ? `${arrowX}px` : '',
      top: arrowY != null ? `${arrowY}px` : '',
      right: '',
      bottom: '',
      [staticSide]: '-4px',
    });
  });

}

function hideTooltip() {
  const tooltip = document.querySelector('#tooltip');
  tooltip.style.display = 'none';
}
