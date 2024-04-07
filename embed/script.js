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

function showTooltip(event) {
  const tooltip = document.querySelector('#tooltip');
  tooltip.style.display = 'block';
  update(event.target);
}

function Event(props) {
  let dto = props.dto;
  const shared_classes = "w-fit outline outline-1 rounded-sm px-1 m-1 bg-slate-800"
  switch (dto.type) {
    case "Print":
      if (!!dto.color && !!dto.bg_color) {
        return html`<span style="color: ${dto.color}; background-color: ${dto.bg_color}">${dto.string}</span>`
      } else if (!!dto.color) {
        return html`<span style="color: ${dto.color}">${dto.string}</span>`
      } else if (!!dto.bg_color) {
        return html`<span style="background-color: ${dto.bg_color}">${dto.string}</span>`
      } else {
        return html`<span>${dto.string}</span>`;
      }
    case "GenericEscape": {
      let svg = dto.icon_svg ? html`<span class="inline-block align-middle" dangerouslySetInnerHTML=${{ __html: dto.icon_svg}}/>` : html``;
      let title = dto.title ? html`<span>${dto.title}</span>` : ``;
      return html`<div
        data-tooltip=${dto.tooltip}
        data-rawbytes=${dto.raw_bytes}
        onmouseenter=${showTooltip}
        onmouseleave=${hideTooltip}
        onfocus=${showTooltip}
        onblur=${hideTooltip}
        class="inline-block outline-slate-400 ${shared_classes} space-x-1"
        >
          ${svg}
          ${title}
        </div>`;
    }
    case "ColorEscape": {
      let svg = dto.icon_svg ? html`<span class="inline-block align-middle" dangerouslySetInnerHTML=${{ __html: dto.icon_svg}}/>` : html``;
      let title = dto.title ? html`<span>${dto.title}</span>` : ``;
      return html`<div
        data-tooltip=${dto.tooltip}
        data-rawbytes=${dto.raw_bytes}
        onmouseenter=${showTooltip}
        onmouseleave=${hideTooltip}
        onfocus=${showTooltip}
        onblur=${hideTooltip}
        class="inline-block outline-[${dto.color}] ${shared_classes} space-x-1"
        >
          ${svg}
          ${title}
        </div>`;
    }
    case "InvisibleLineBreak": {
      return html`<div/>`;
    }
    case "LineBreak": {
      return html`
      <span class="${shared_classes} outline-slate-500 text-xs">
        ${dto.title}
      </span>`;
    }
    case "Disconnected": {
      return html`<div class="${shared_classes} outline-red-500">
        Disconnected
      </div>`;
    }
    default:
      return html`<span class="inline-block ${shared_classes} outline-slate-500">${dto.type}</span>`;
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
