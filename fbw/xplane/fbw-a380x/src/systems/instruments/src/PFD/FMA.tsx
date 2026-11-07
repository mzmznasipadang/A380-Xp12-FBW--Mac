/* eslint-disable no-dupe-else-if */
/* eslint-disable no-constant-condition */
import {
  ViewProps,
  BusSignal,
  View,
  Bus,
  UI,
  Derived,
  Signal,
  Readable,
  Logic,
  ViewNode,
} from 'xplane-js/ui';
import { Arinc429Values } from './shared/ArincValueProvider';
import { PFDSimvars } from './shared/PFDSimvarPublisher';
import {
  Arinc429ConsumerSubject,
  Arinc429LocalVarConsumerSubject,
  Arinc429Word,
  ArincEventBus,
} from '@flybywiresim/fbw-sdk';
import { DmcLogicEvents } from '../MsfsAvionicsCommon/providers/DmcPublisher';
import { PrimFgBusBaseEvents } from '@shared/publishers/PrimFgPublisher';
import { FlashOneHertz } from '../MsfsAvionicsCommon/FlashingElementUtils';
import {
  A1A2Messages,
  A3Messages,
  B1Messages,
  BC3Messages,
  C1Messages,
  computeA1A2Message,
  computeA3Message,
  computeB1Message,
  computeBC3Message,
  computeC1Message,
  computeD1D2Message,
  D1D2Messages,
} from './FMADefinitions';
import { FcdcBusBaseEvents } from '@shared/publishers/FcdcPublisher';
import { FcuEfisCpBusEvents } from '../../../shared/src/publishers/EfisCpBusPublisher';
import { getDisplayIndex } from './PFD';
import { EcuBusEvents } from '@shared/publishers/EcuPublisher';

abstract class ShowForSecondsComponent<T extends ViewProps> extends View<T> {
  private timeout: number = 0;

  private readonly displayTimeInSeconds: number;

  protected modeChangedPathRef = UI.ref<SVGPathElement>();

  protected isShown = false;

  protected constructor(props: T, displayTimeInSeconds: number) {
    super(props);
    this.displayTimeInSeconds = displayTimeInSeconds;
  }

  public displayModeChangedPath = (cancel = false) => {
    if (cancel || !this.isShown) {
      clearTimeout(this.timeout);
      this.modeChangedPathRef.target.classList.remove('ModeChangedPath');
    } else {
      this.modeChangedPathRef.target.classList.add('ModeChangedPath');
      clearTimeout(this.timeout);
      this.timeout = setTimeout(() => {
        this.modeChangedPathRef.target.classList.remove('ModeChangedPath');
      }, this.displayTimeInSeconds * 1000) as unknown as number;
    }
  };
}

export class FMA extends View<{
  readonly bus: ArincEventBus;
  readonly isAttExcessive: Readable<boolean>;
}> {
  private sub = this.props.bus.inbox<
    PFDSimvars & Arinc429Values & DmcLogicEvents & PrimFgBusBaseEvents & FcdcBusBaseEvents & EcuBusEvents
  >();

  private primFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_1'));

  private primFgDiscreteWord2 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_2'));

  private primFgDiscreteWord3 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_3'));

  private primFgDiscreteWord4 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_4'));

  private primFgDiscreteWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_6'));

  private primFgAtsDiscreteWord = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_ats_discrete_word'));

  private primFgAtsFmaDiscreteWord = Arinc429LocalVarConsumerSubject.create(
    this.sub.topic('prim_fg_ats_fma_discrete_word'),
  );

  private readonly fcdcDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('fcdc_discrete_word_1'));

  private ecu1MaintenanceWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('ecu_maintenance_word_6_1'));
  private ecu2MaintenanceWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('ecu_maintenance_word_6_2'));
  private ecu3MaintenanceWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('ecu_maintenance_word_6_3'));
  private ecu4MaintenanceWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('ecu_maintenance_word_6_4'));

  private readonly ap1Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(11, false));

  private readonly ap2Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(12, false));

  private readonly athrEngaged = this.primFgAtsDiscreteWord.derive((word) => word.bitValueOr(11, false));

  private readonly athrActive = this.primFgAtsDiscreteWord.derive((word) => word.bitValueOr(12, false));

  private machPresel = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_presel_mach'));

  private speedPresel = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_presel_speed'));

  private setHoldSpeed = BusSignal.from(this.sub.topic('setHoldSpeed'), false);

  private tdReached = BusSignal.from(this.sub.topic('tdReached'), false);

  private firstBorderRef = UI.ref<SVGPathElement>();

  private secondBorderRef = UI.ref<SVGPathElement>();

  private readonly radioHeight = BusSignal.from(this.sub.topic('chosenRa'), Arinc429Word.empty());

  private readonly altitude = Arinc429ConsumerSubject.create(
    this.props.bus.getArincSubscriber<Arinc429Values>().on('altitudeAr'),
  );

  private readonly landingElevation = BusSignal.from(this.sub.topic('landingElevation'), Arinc429Word.empty());

  private readonly fwcFlightPhase = BusSignal.from(this.sub.topic('fwcFlightPhase'), 0);

  private readonly btvExitMissed = BusSignal.from(this.sub.topic('btvExitMissed'), false);

  private readonly autoBrakeActive = BusSignal.from(this.sub.topic('autoBrakeActive'), false);

  private readonly autoBrakeMode = BusSignal.from(this.sub.topic('autoBrakeMode'), 0);

  private readonly B1Message = this.primFgDiscreteWord3.derive((primFgDiscreteWord3) =>
    computeB1Message(primFgDiscreteWord3),
  );

  private readonly disconnectApForLdg = Derived.from(
    ([ap1, ap2, ra, altitude, landingElevation, B1Message]) => {
      return (
        (ap1 || ap2) &&
        (ra.isNormalOperation() ? ra.value <= 150 : altitude.valueOr(Infinity) - landingElevation.valueOr(0) <= 150) &&
        (B1Message === B1Messages.DES ||
          B1Message === B1Messages.OP_DES ||
          B1Message === B1Messages.FPA ||
          B1Message === B1Messages.VS ||
          B1Message === B1Messages.APP_DES)
      );
    },
    this.ap1Engaged,
    this.ap2Engaged,
    this.radioHeight,
    this.altitude,
    this.landingElevation,
    this.B1Message,
  );

  private readonly thrustLocked = Derived.from(
    ([ecu1MaintenanceWord6, ecu2MaintenanceWord6, ecu3MaintenanceWord6, ecu4MaintenanceWord6]) =>
      ecu1MaintenanceWord6.bitValueOr(12, false) ||
      ecu2MaintenanceWord6.bitValueOr(12, false) ||
      ecu3MaintenanceWord6.bitValueOr(12, false) ||
      ecu4MaintenanceWord6.bitValueOr(12, false),
    this.ecu1MaintenanceWord6,
    this.ecu2MaintenanceWord6,
    this.ecu3MaintenanceWord6,
    this.ecu4MaintenanceWord6,
  );

  private readonly BC3Message = Derived.from(
    ([
      isAttExcessive,
      primFgDiscreteWord2,
      setHoldSpeed,
      fcdcDiscreteWord1,
      fwcFlightPhase,
      primFgDiscreteWord6,
      tdReached,
      disconnectApForLdg,
      btvExitMissed,
      thrustLocked,
    ]) => {
      return computeBC3Message(
        isAttExcessive,
        setHoldSpeed,
        fcdcDiscreteWord1,
        fwcFlightPhase,
        tdReached,
        disconnectApForLdg,
        btvExitMissed,
        primFgDiscreteWord2,
        primFgDiscreteWord6,
        thrustLocked,
      );
    },
    this.props.isAttExcessive,
    this.primFgDiscreteWord2,
    this.setHoldSpeed,
    this.fcdcDiscreteWord1,
    this.fwcFlightPhase,
    this.primFgDiscreteWord6,
    this.tdReached,
    this.disconnectApForLdg,
    this.btvExitMissed,
    this.thrustLocked,
  );

  private readonly A1A2Message = Derived.from(
    ([athrEngaged, athrActive, primFgAtsFmaDiscreteWord, autoBrakeActive, autoBrakeMode]) => {
      return computeA1A2Message(athrEngaged, athrActive, primFgAtsFmaDiscreteWord, autoBrakeActive, autoBrakeMode);
    },
    this.athrEngaged,
    this.athrActive,
    this.primFgAtsFmaDiscreteWord,
    this.autoBrakeActive,
    this.autoBrakeMode,
  );

  private readonly A3Message = Derived.from(
    ([primFgAtsFmaDiscreteWord, autoBrakeActive, autoBrakeMode, thrustLocked]) => {
      return computeA3Message(primFgAtsFmaDiscreteWord, thrustLocked, autoBrakeActive, autoBrakeMode);
    },
    this.primFgAtsFmaDiscreteWord,
    this.autoBrakeActive,
    this.autoBrakeMode,
    this.thrustLocked,
  );

  private readonly sharedModeActive = Derived.from(
    ([primFgDiscreteWord1, primFgDiscreteWord3, primFgDiscreteWord4]) => {
      const rollOutActive = primFgDiscreteWord4.bitValueOr(26, false);
      const flareActive = primFgDiscreteWord3.bitValueOr(24, false);
      const landActive = primFgDiscreteWord1.bitValueOr(23, false);

      return landActive || rollOutActive || flareActive;
    },
    this.primFgDiscreteWord1,
    this.primFgDiscreteWord3,
    this.primFgDiscreteWord4,
  );

  private handleFMABorders() {
    const sharedModeActive = this.sharedModeActive.get();
    const BC3Message = this.BC3Message.get() !== 0;

    const engineMessage = this.A3Message.get();
    const AB3Message =
      (this.machPresel.get().isNormalOperation() || this.speedPresel.get().isNormalOperation()) &&
      !BC3Message &&
      engineMessage === A3Messages.NONE;
    let secondBorder: string;
    if (sharedModeActive && !this.props.isAttExcessive.get()) {
      secondBorder = '';
    } else if (BC3Message) {
      secondBorder = 'm66.241 0.33732v15.766';
    } else {
      secondBorder = 'm66.241 0.33732v20.864';
    }

    let firstBorder: string;
    if (AB3Message && !this.props.isAttExcessive.get()) {
      firstBorder = 'm33.117 0.33732v15.766';
    } else {
      firstBorder = 'm33.117 0.33732v20.864';
    }

    this.firstBorderRef.target.setAttribute('d', firstBorder);
    this.secondBorderRef.target.setAttribute('d', secondBorder);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.BC3Message.watch(() => {
      this.handleFMABorders();
    }, true);

    this.sharedModeActive.watch(() => {
      this.handleFMABorders();
    }, true);

    this.A3Message.watch(() => {
      this.handleFMABorders();
    }, true);

    this.machPresel.watch(() => {
      this.handleFMABorders();
    }, true);

    this.speedPresel.watch(() => {
      this.handleFMABorders();
    }, true);
  }

  render(): ViewNode {
    return (
      <g id="FMA">
        <g class="NormalStroke Grey">
          <path ref={this.firstBorderRef} />
          <path ref={this.secondBorderRef} />
          <path d="m102.52 0.33732v20.864" />
          <path d="m133.72 0.33732v20.864" />
        </g>

        <Row1
          bus={this.props.bus}
          isAttExcessive={this.props.isAttExcessive}
          A1A2CellMessage={this.A1A2Message}
          B1CellMessage={this.B1Message}
        />
        <Row2 bus={this.props.bus} isAttExcessive={this.props.isAttExcessive} A1A2CellMessage={this.A1A2Message} />
        <Row3
          bus={this.props.bus}
          isAttExcessive={this.props.isAttExcessive}
          BC3Message={this.BC3Message}
          A3Message={this.A3Message}
        />
      </g>
    );
  }
}

class Row1 extends View<{
  readonly bus: Bus;
  readonly isAttExcessive: Readable<boolean>;
  readonly A1A2CellMessage: Readable<number>;
  readonly B1CellMessage: Readable<number>;
}> {
  private b1Cell = UI.ref<B1Cell>();

  private c1Cell = UI.ref<C1Cell>();

  private D1D2Cell = UI.ref<D1D2Cell>();

  private BC1Cell = UI.ref<BC1Cell>();

  private cellsToHide = UI.ref<SVGGElement>();

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.isAttExcessive.watch((a) => {
      if (a) {
        this.cellsToHide.target.style.display = 'none';
        this.b1Cell.target.displayModeChangedPath(true);
        this.c1Cell.target.displayModeChangedPath(true);
        this.BC1Cell.target.displayModeChangedPath(true);
      } else {
        this.cellsToHide.target.style.display = 'inline';
        this.b1Cell.target.displayModeChangedPath();
        this.c1Cell.target.displayModeChangedPath();
        this.BC1Cell.target.displayModeChangedPath();
      }
    });
  }

  render(): ViewNode {
    return (
      <g>
        <A1A2Cell bus={this.props.bus} A1A2CellMessage={this.props.A1A2CellMessage} />

        <g ref={this.cellsToHide}>
          <B1Cell ref={this.b1Cell} bus={this.props.bus} B1Message={this.props.B1CellMessage} />
          <C1Cell ref={this.c1Cell} bus={this.props.bus} />
          <D1D2Cell ref={this.D1D2Cell} bus={this.props.bus} />
          <BC1Cell ref={this.BC1Cell} bus={this.props.bus} />
        </g>
        <E1Cell bus={this.props.bus} />
      </g>
    );
  }
}

class Row2 extends View<{
  bus: Bus;
  isAttExcessive: Readable<boolean>;
  A1A2CellMessage: Readable<number>;
}> {
  private cellsToHide = UI.ref<SVGGElement>();

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.isAttExcessive.watch((a) => {
      if (a) {
        this.cellsToHide.target.style.display = 'none';
      } else {
        this.cellsToHide.target.style.display = 'inline';
      }
    });
  }

  render(): ViewNode {
    return (
      <g>
        <A2Cell bus={this.props.bus} A1A2CellMessage={this.props.A1A2CellMessage} />
        <g ref={this.cellsToHide}>
          <B2Cell bus={this.props.bus} />
          <C2Cell bus={this.props.bus} />
        </g>
        <E2Cell bus={this.props.bus} />
      </g>
    );
  }
}

class A2Cell extends View<{ bus: Bus; A1A2CellMessage: Readable<A1A2Messages> }> {
  private text = Signal.of('');

  private className = Signal.of('FontMediumSmaller MiddleAlign Cyan');

  private autoBrkRef = UI.ref<SVGTextElement>();

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    const sub = this.props.bus.inbox<PFDSimvars>();

    sub
      .topic('autoBrakeMode')
      .distinct()
      .listen((am) => {
        switch (am) {
          case 0:
            this.text.set('');
            break;
          case 1:
            this.text.set('BTV ');
            break;
          case 2:
            this.text.set('BRK LO ');
            break;
          case 3:
            this.text.set('BRK 2 ');
            break;
          case 4:
            this.text.set('BRK 3 ');
            break;
          case 5:
            this.text.set('BRK HI ');
            break;
          default:
            this.text.set('');
            break;
        }
      });

    sub
      .topic('autoBrakeActive')
      .distinct()
      .listen((am) => {
        if (am) {
          this.autoBrkRef.target.style.visibility = 'hidden';
        } else {
          this.autoBrkRef.target.style.visibility = 'visible';
        }
      });

    this.props.A1A2CellMessage.watch((message) => {
      // ATHR mode overrides BRK LO and MED memo
      if (message > A1A2Messages.NONE && message <= A1A2Messages.MAN_THR) {
        this.autoBrkRef.target.style.visibility = 'hidden';
      } else {
        this.autoBrkRef.target.style.visibility = 'visible';
      }
    }, true);
  }

  render(): ViewNode {
    return (
      <text ref={this.autoBrkRef} class={this.className} x="16.782249" y="14.329653" style="white-space: pre">
        {this.text}
      </text>
    );
  }
}

class Row3 extends View<{
  readonly bus: ArincEventBus;
  readonly isAttExcessive: Readable<boolean>;
  readonly BC3Message: Readable<BC3Messages>;
  readonly A3Message: Readable<A3Messages>;
}> {
  private cellsToHide = UI.ref<SVGGElement>();

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.isAttExcessive.watch((a) => {
      if (a) {
        this.cellsToHide.target.style.display = 'none';
      } else {
        this.cellsToHide.target.style.display = 'inline';
      }
    });
  }

  render(): ViewNode {
    return (
      <g>
        <A3Cell bus={this.props.bus} A3Message={this.props.A3Message} />
        <g ref={this.cellsToHide}>
          <AB3Cell bus={this.props.bus} A3Message={this.props.A3Message} />
          <D3Cell bus={this.props.bus} />
        </g>
        <BC3Cell BC3Message={this.props.BC3Message} />
        <E3Cell bus={this.props.bus} />
      </g>
    );
  }
}

interface CellProps extends ViewProps {
  bus: Bus;
}

interface A1A2CellProps extends CellProps {
  A1A2CellMessage: Readable<A1A2Messages>;
}

class A1A2Cell extends ShowForSecondsComponent<A1A2CellProps> {
  private readonly sub = this.props.bus.inbox<PFDSimvars>();

  private cellRef = UI.ref<SVGGElement>();

  private flexTemp = BusSignal.from(this.sub.topic('flexTemp'), 0);

  constructor(props: A1A2CellProps) {
    super(props, 10);
  }

  private setText() {
    let text: string = '';
    this.isShown = true;

    switch (this.props.A1A2CellMessage.get()) {
      case A1A2Messages.MAN_TOGA:
        this.displayModeChangedPath(true);
        text = `
                                <path class="NormalStroke White" d="m25.114 1.8143v13.506h-16.952v-13.506z" />
                                <text class="FontMedium MiddleAlign White" x="17.052249" y="7.1280665">MAN</text>
                                <text class="FontMedium MiddleAlign White" x="16.869141" y="14.351689">TOGA</text>
                            `;
        break;
      case A1A2Messages.MAN_GA_SOFT:
        this.displayModeChangedPath(true);
        text = `<g>
                                <path class="NormalStroke White" d="m31.521 1.8143v13.506h-30.217v-13.506z" />
                                <text class="FontMedium MiddleAlign White" x="17.052249" y="7.1280665">MAN</text>
                                <text class="FontMedium MiddleAlign White" x="16.869141" y="14.351689">GA SOFT</text>
                            </g>`;
        break;
      case A1A2Messages.MAN_FLEX: {
        this.displayModeChangedPath(true);
        const FlexTemp = Math.round(this.flexTemp.get());
        const FlexText = FlexTemp.toString();
        text = `<g>
                                <path class="NormalStroke White" d="m29.821 1.8143v13.506h-24.517v-13.506z" />
                                <text class="FontMedium MiddleAlign White" x="17.052249" y="7.1280665">MAN</text>
                                <text class="FontMedium MiddleAlign White" x="11.669141" y="14.351689">FLX</text>
                                <text class="FontMedium MiddleAlign Cyan" x="20.599141" y="14.851689">+</text>
                                <text class="FontMedium MiddleAlign Cyan" x="26.099141" y="14.351689">
                                ${FlexText}
                                </text>
                            </g>`;

        break;
      }
      case A1A2Messages.MAN_MCT:
        this.displayModeChangedPath(true);
        text = `<g>
                                <path class="NormalStroke White" d="m25.114 1.8143v13.506h-16.952v-13.506z" />
                                <text class="FontMedium MiddleAlign White" x="17.052249" y="7.1280665">MAN</text>
                                <text class="FontMedium MiddleAlign White" x="16.869141" y="14.351689">MCT</text>
                            </g>`;
        break;
      case A1A2Messages.MAN_THR:
        this.displayModeChangedPath(true);
        text = `<g>
                                <path class="NormalStroke Amber" d="m25.114 1.8143v13.506h-16.952v-13.506z" />
                                <text class="FontMedium MiddleAlign White" x="17.052249" y="7.1280665">MAN</text>
                                <text class="FontMedium MiddleAlign White" x="16.869141" y="14.351689">THR</text>
                            </g>`;
        break;
      case A1A2Messages.SPEED:
        text = '<text  class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">SPEED</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.MACH:
        text = '<text  class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">MACH</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.THR_MCT:
        text = '<text  class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">THR MCT</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.THR_CLB:
        text = '<text  class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">THR CLB</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.THR_LVR:
        text = '<text  class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">THR LVR</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.THR_IDLE:
        text = '<text class="FontMediumSmaller MiddleAlign Green" x="16.782249" y="7.1280665">THR IDLE</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.A_FLOOR:
        this.displayModeChangedPath(true);
        text = `<g>
                                <path class="NormalStroke Amber BlinkInfinite" d="m0.70556 1.8143h30.927v6.0476h-30.927z" />
                                <text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">A.FLOOR</text>
                            </g>`;
        break;
      case A1A2Messages.TOGA_LK:
        this.displayModeChangedPath(true);
        text = `<g>
                                <path class="NormalStroke Amber BlinkInfinite" d="m0.70556 1.8143h30.927v6.0476h-30.927z" />
                                <text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">TOGA LK</text>
                            </g>`;
        break;
      case A1A2Messages.BTV:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BTV</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.BRK_LO:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BRK LO</text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.BRK_2:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BRK 2 </text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.BRK_3:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BRK 3 </text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.BRK_HI:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BRK HI </text>';
        this.displayModeChangedPath();
        break;
      case A1A2Messages.BRK_RTO:
        text = '<text class="FontMedium MiddleAlign Green" x="16.782249" y="7.1280665">BRK RTO</text>';
        this.displayModeChangedPath();
        break;
      default:
        text = '';
        this.isShown = false;
        this.displayModeChangedPath(true);
    }

    this.cellRef.target.innerHTML = text;
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.A1A2CellMessage.watch(() => {
      this.setText();
    }, true);

    this.flexTemp.watch(() => {
      this.setText();
    }, true);
  }

  render(): ViewNode {
    return (
      <>
        <path
          ref={this.modeChangedPathRef}
          visibility="hidden"
          class="NormalStroke White"
          d="m3.3 1.8143h27.127v6.0476h-27.127z"
        />
        <g ref={this.cellRef} />
      </>
    );
  }
}

interface A3CellProps extends CellProps {
  A3Message: Readable<A3Messages>;
}

class A3Cell extends View<A3CellProps> {
  private classSub = Signal.of('');

  private textSub = Signal.of('');

  private onUpdateAthrModeMessage(message: A3Messages) {
    let text: string = '';
    let className: string = '';
    switch (message) {
      case A3Messages.THR_LK:
        text = 'THR LK';
        className = 'FontMedium Amber';
        break;
      case A3Messages.LVR_TOGA:
        text = 'LVR TOGA';
        className = 'FontMedium White';
        break;
      case A3Messages.LVR_CLB:
        text = 'LVR CLB';
        className = 'FontMedium White';
        break;
      case A3Messages.LVR_MCT:
        text = 'LVR MCT';
        className = 'FontMedium White';
        break;
      case A3Messages.LVR_ASYM:
        text = 'LVR ASYM';
        className = 'FontMedium Amber';
        break;
      case A3Messages.BRK_RTO:
        text = 'BRK RTO';
        className = 'FontMediumSmaller Cyan';
        break;
      default:
        text = '';
    }

    this.textSub.set(text);
    this.classSub.set(`MiddleAlign ${className}`);
  }

  private readonly shouldFlash = this.props.A3Message.derive(
    (A3Message) => A3Message !== A3Messages.BRK_RTO && A3Message !== A3Messages.LVR_ASYM,
  );

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.A3Message.watch((a3) => {
      this.onUpdateAthrModeMessage(a3);
    }, true);
  }

  render(): ViewNode {
    return (
      <FlashOneHertz bus={this.props.bus} flashDuration={Infinity} flashing={this.shouldFlash}>
        <text class={this.classSub} x="16.989958" y="21.641243">
          {this.textSub}
        </text>
      </FlashOneHertz>
    );
  }
}

interface AB3CellProps extends CellProps {
  A3Message: Readable<A3Messages>;
}

class AB3Cell extends View<AB3CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private machPresel = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_presel_mach'));

  private speedPresel = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_presel_speed'));

  private readonly textSub = Derived.from(
    ([machPresel, speedPresel, A3Message]) => {
      if (A3Message !== A3Messages.NONE) {
        return '';
      } else if (speedPresel.isNormalOperation() && !machPresel.isNormalOperation()) {
        return `SPEED SEL ${speedPresel.value}`;
      } else if (!speedPresel.isNormalOperation() && machPresel.isNormalOperation()) {
        return `MACH SEL ${machPresel.value.toFixed(2)}`;
      } else {
        return '';
      }
    },
    this.machPresel,
    this.speedPresel,
    this.props.A3Message,
  );

  render(): ViewNode {
    return (
      <text class="FontMedium MiddleAlign Cyan" x="35.434673" y="21.656223">
        {this.textSub}
      </text>
    );
  }
}

interface B1CellProps extends CellProps {
  B1Message: Readable<B1Messages>;
}

class B1Cell extends ShowForSecondsComponent<B1CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord3 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_3'));

  private primFgDiscreteWord5 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_5'));

  private primFgDiscreteWord6 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_6'));

  private primFgSelectedVs = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_selected_vertical_speed'));

  private primFgSelectedFpa = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_selected_flight_path_angle'));

  private readonly fmaTextRef = UI.ref<SVGTextElement>();

  private readonly tcasLargeBoxDemand = this.primFgDiscreteWord6.derive((word) => word.bitValueOr(11, false));

  private readonly targetNotHeld = this.primFgDiscreteWord5.derive((word) => word.bitValueOr(29, false));

  private readonly text = Derived.from(
    ([B1Message, primFgSelectedFpa]) => {
      this.isShown = true;

      switch (B1Message) {
        case B1Messages.NONE:
          this.isShown = false;
          return '';

        case B1Messages.GS:
          return 'G/S';
        case B1Messages.F_GS:
          return 'F-G/S';
        case B1Messages.GS_STAR:
          return 'G/S*';
        case B1Messages.F_GS_STAR:
          return 'F-G/S*';
        case B1Messages.SRS:
          return 'SRS';
        case B1Messages.TCAS:
          return 'TCAS';
        case B1Messages.APP_DES:
          return 'APP-DES';
        case B1Messages.DES:
          return 'DES';
        case B1Messages.OP_DES:
          return 'OP DES';
        case B1Messages.CLB:
          return 'CLB';
        case B1Messages.OP_CLB:
          return 'OP CLB';
        case B1Messages.ALT:
          return 'ALT';
        case B1Messages.ALT_STAR:
          return 'ALT*';
        case B1Messages.ALT_CST:
          return 'ALT CST';
        case B1Messages.ALT_CST_STAR:
          return 'ALT CST*';
        case B1Messages.ALT_CRZ:
          return 'ALT CRZ';
        case B1Messages.ALT_CRZ_STAR:
          return 'ALT CRZ*';
        case B1Messages.FPA: {
          let text = 'FPA';
          const fpaValue = primFgSelectedFpa.value;

          // if FPA is 0 give it an empty space for where the '+' and '-' will be.
          if (!(primFgSelectedFpa.isNoComputedData() || primFgSelectedFpa.isFailureWarning()) && fpaValue === 0) {
            text += ' ';
          }
          return text;
        }
        case B1Messages.VS:
          return 'V/S';
        default:
          this.isShown = false;
          return '';
      }
    },
    this.props.B1Message,
    this.primFgSelectedFpa,
  );

  private readonly additionalText = Derived.from(
    ([B1Message, primFgSelectedVs, primFgSelectedFpa]) => {
      if (B1Message === B1Messages.FPA) {
        if (!(primFgSelectedFpa.isNoComputedData() || primFgSelectedFpa.isFailureWarning())) {
          const fpaValue = primFgSelectedFpa.value;
          return `${fpaValue > 0 ? '+' : ''}${(Math.round(fpaValue * 10) / 10).toFixed(1)}°`;
        } else {
          return '-----';
        }
      } else if (B1Message === B1Messages.VS) {
        if (!(primFgSelectedVs.isNoComputedData() || primFgSelectedVs.isFailureWarning())) {
          const vsValue = primFgSelectedVs.value;
          return `${vsValue > 0 ? '+' : ''}${Math.round(vsValue).toString()}`.padStart(5, '\xa0');
        } else {
          return '-----';
        }
      } else {
        return '';
      }
    },
    this.props.B1Message,
    this.primFgSelectedVs,
    this.primFgSelectedFpa,
  );

  private readonly inSpeedProtection = Derived.from(
    ([text, targetNotHeld]) => {
      return targetNotHeld && text !== '';
    },
    this.text,
    this.targetNotHeld,
  );

  private readonly boxClassSub = this.inSpeedProtection.derive((inSpeedProtection) =>
    inSpeedProtection ? 'NormalStroke None' : 'NormalStroke White',
  );

  private readonly boxPathStringSub = Derived.from(
    ([tcasLargeBoxDemand, primFgDiscreteWord3]) => {
      return tcasLargeBoxDemand && primFgDiscreteWord3.bitValueOr(25, false)
        ? 'm35.756 1.8143h27.918v13.506h-27.918z'
        : 'm35.756 1.8143h27.918v6.0476h-27.918z';
    },
    this.tcasLargeBoxDemand,
    this.primFgDiscreteWord3,
  );

  private readonly activeVerticalModeClassSub = Derived.from(
    ([word, b1]) => {
      // VS FPA has a smaller font than the other active modes
      const fpaMode = word.bitValueOr(18, false);
      const vsMode = word.bitValueOr(17, false);

      // ALT CRZ* also has a smaller font, as it otherwise would be too large for the box.
      const altCstrApplicable = word.bitValueOr(28, false);
      const altIsCrzAlt = word.bitValueOr(29, false);
      const altAcqMode = word.bitValueOr(19, false);
      const crzStar = b1 === B1Messages.ALT_CRZ_STAR;

      return vsMode || fpaMode || crzStar || (altAcqMode && !altCstrApplicable && altIsCrzAlt)
        ? 'FontMediumSmaller MiddleAlign Green'
        : 'FontMedium MiddleAlign Green';
    },
    this.primFgDiscreteWord3,
    this.props.B1Message,
  );

  constructor(props: B1CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.text.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path ref={this.modeChangedPathRef} class={this.boxClassSub} visibility="hidden" d={this.boxPathStringSub} />

        <FlashOneHertz bus={this.props.bus} flashDuration={Infinity} visible={this.inSpeedProtection}>
          <path class="NormalStroke Amber" d="m34.656 1.8143h29.918v6.0476h-29.918z" />
        </FlashOneHertz>

        <text
          ref={this.fmaTextRef}
          style="white-space: pre"
          class={this.activeVerticalModeClassSub}
          x="49.921795"
          y="7.1040988"
        >
          <tspan>{this.text}</tspan>
          <FlashOneHertz
            bus={this.props.bus}
            flashDuration={Infinity}
            flashing={this.inSpeedProtection}
            className1={'Cyan'}
            className2={'DimmedCyan Fill'}
          >
            <tspan xml:space="preserve">{this.additionalText}</tspan>
          </FlashOneHertz>
        </text>
      </g>
    );
  }
}

class B2Cell extends View<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord2 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_2'));

  private primFgDiscreteWord3 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_3'));

  private classSub = Derived.from(
    ([primFgDiscreteWord2, primFgDiscreteWord3]) => {
      const altAcqArmed = primFgDiscreteWord2.bitValueOr(11, false);

      const altCstrApplicable = primFgDiscreteWord3.bitValueOr(28, false);

      return altAcqArmed && altCstrApplicable
        ? 'FontMediumSmaller MiddleAlign Magenta'
        : 'FontMediumSmaller MiddleAlign Cyan';
    },
    this.primFgDiscreteWord2,
    this.primFgDiscreteWord3,
  );

  private text1Sub = Derived.from(
    ([primFgDiscreteWord2, primFgDiscreteWord3]) => {
      const altAcqArmed = primFgDiscreteWord2.bitValueOr(11, false);
      const clbArmed = primFgDiscreteWord2.bitValueOr(15, false);
      const desArmed = primFgDiscreteWord2.bitValueOr(16, false);
      const opClbArmed = primFgDiscreteWord2.bitValueOr(17, false);

      const gsArmed = primFgDiscreteWord2.bitValueOr(13, false);
      const altCstrApplicable = primFgDiscreteWord3.bitValueOr(28, false);
      const altIsCrzAlt = primFgDiscreteWord3.bitValueOr(29, false);

      if (opClbArmed) {
        return '      OP CLB';
      } else if (clbArmed) {
        return '      CLB';
      } else if (desArmed) {
        return gsArmed ? 'DES ' : '      DES';
      } else if (altAcqArmed && altCstrApplicable) {
        return gsArmed ? 'ALT ' : '      ALT';
      } else if (altAcqArmed && altIsCrzAlt) {
        return '     ALT CRZ';
      } else if (altAcqArmed) {
        return gsArmed ? 'ALT ' : '      ALT';
      } else if (gsArmed) {
        return '      G/S';
      } else {
        return '';
      }
    },
    this.primFgDiscreteWord2,
    this.primFgDiscreteWord3,
  );

  private text2Sub = Derived.from(
    ([primFgDiscreteWord2, text1Sub]) => {
      const gsArmed = primFgDiscreteWord2.bitValueOr(13, false);

      //TODO Improve this logic, very ugly
      if (gsArmed && text1Sub !== '      G/S') {
        return '  G/S';
      } else if (primFgDiscreteWord2.bitValueOr(14, false)) {
        return 'APP-DES';
      } else {
        return '';
      }
    },
    this.primFgDiscreteWord2,
    this.text1Sub,
  );

  render(): ViewNode {
    return (
      <g>
        <text class={this.classSub} style="white-space: pre" x="40.777474" y="13.629653">
          {this.text1Sub}
        </text>
        <text style="white-space: pre" class="FontMediumSmaller MiddleAlign Cyan" x="56.19803" y="13.629653">
          {this.text2Sub}
        </text>
      </g>
    );
  }
}

class C1Cell extends ShowForSecondsComponent<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord4 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_4'));

  private readonly message = this.primFgDiscreteWord4.derive((primFgDiscreteWord4) =>
    computeC1Message(primFgDiscreteWord4),
  );

  private readonly text = this.message.derive((C1Message) => {
    this.isShown = true;
    switch (C1Message) {
      case C1Messages.GA_TRK:
        return 'GA TRK';
      case C1Messages.LOC_BC_STAR:
        return 'LOC B/C*';
      case C1Messages.LOC_STAR:
        return 'LOC *';
      case C1Messages.F_LOC_STAR:
        return 'F-LOC *';
      case C1Messages.HDG:
        return 'HDG';
      case C1Messages.RWY:
        return 'RWY';
      case C1Messages.RWY_TRK:
        return 'RWY TRK';
      case C1Messages.TRACK:
        return 'TRACK';
      case C1Messages.LOC_BC:
        return 'LOC B/C';
      case C1Messages.LOC:
        return 'LOC';
      case C1Messages.F_LOC:
        return 'F-LOC';
      case C1Messages.NAV:
        return 'NAV';
      default:
        this.isShown = false;
        return '';
    }
  });

  constructor(props: CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.text.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path
          ref={this.modeChangedPathRef}
          class="NormalStroke White"
          visibility="hidden"
          d="m99.87 1.8143v6.0476h-31.075l1e-6 -6.0476z"
        />
        <text class="FontMedium MiddleAlign Green" x="84.856567" y="6.9873109">
          {this.text}
        </text>
      </g>
    );
  }
}

class C2Cell extends View<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord2 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_2'));

  private primFgDiscreteWord4 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_4'));

  private readonly text = Derived.from(
    ([primFgDiscreteWord2, primFgDiscreteWord4]) => {
      const navArmed = primFgDiscreteWord2.bitValueOr(22, false);
      const locArmed = primFgDiscreteWord2.bitValueOr(23, false);
      const rwyArmed = primFgDiscreteWord2.bitValueOr(24, false);
      const backbeamMode = primFgDiscreteWord4.bitValueOr(29, false);

      if (locArmed && backbeamMode) {
        return 'LOC B/C';
      } else if (locArmed) {
        return 'LOC';
      } else if (false) {
        return 'F-LOC';
      } else if (rwyArmed) {
        return 'RWY' + (navArmed ? '  NAV' : '');
      } else if (navArmed) {
        return 'NAV';
      }
    },
    this.primFgDiscreteWord2,
    this.primFgDiscreteWord4,
  );

  render(): ViewNode {
    return (
      <text style="white-space: pre" class="FontMediumSmaller MiddleAlign Cyan" x="84.234184" y="13.629653">
        {this.text}
      </text>
    );
  }
}

class BC1Cell extends ShowForSecondsComponent<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_1'));

  private primFgDiscreteWord3 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_3'));

  private primFgDiscreteWord4 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_4'));

  private readonly text = Derived.from(
    ([primFgDiscreteWord1, primFgDiscreteWord3, primFgDiscreteWord4]) => {
      const rollOutActive = primFgDiscreteWord4.bitValueOr(26, false);
      const flareActive = primFgDiscreteWord3.bitValueOr(24, false);
      const landActive = primFgDiscreteWord1.bitValueOr(23, false) && !flareActive && !rollOutActive;

      this.isShown = true;
      if (rollOutActive) {
        return 'ROLL OUT';
      } else if (flareActive) {
        return 'FLARE';
      } else if (landActive) {
        return 'LAND';
      } else {
        this.isShown = false;
        return '';
      }
    },
    this.primFgDiscreteWord1,
    this.primFgDiscreteWord3,
    this.primFgDiscreteWord4,
  );

  constructor(props: CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.text.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path
          ref={this.modeChangedPathRef}
          class="NormalStroke White"
          visibility="hidden"
          d="m50.178 1.8143h35.174v6.0476h-35.174z"
        />
        <text class="FontMedium MiddleAlign Green" x="67.9795" y="6.8893085">
          {this.text}
        </text>
      </g>
    );
  }
}

class BC3Cell extends View<{
  readonly BC3Message: Readable<BC3Messages>;
}> {
  private bc3Cell = UI.ref<SVGTextElement>();

  private classNameSub = Signal.of('');

  private getBC3MessageText(BC3Message: BC3Messages) {
    let text: string;
    let className: string;

    switch (BC3Message) {
      case BC3Messages.USE_MAN_PITCH_TRIM:
        text = 'USE MAN PITCH TRIM';
        className = 'PulseAmber9Seconds Amber';
        break;
      case BC3Messages.FOR_GA_SET_TOGA:
        text = 'FOR GA: SET TOGA';
        className = 'PulseAmber9Seconds Amber';
        break;
      case BC3Messages.DISCONNECT_AP_FOR_LDG:
        text = 'DISCONNECT AP FOR LDG';
        className = 'FontSmall PulseAmber9Seconds Amber';
        break;
      case BC3Messages.TCAS_ARMED:
        text = 'TCAS           ';
        className = 'FontMediumSmaller Cyan';
        break;
      case BC3Messages.TCAS_RA_INHIBITED:
        text = 'TCAS RA INHIBITED';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.TRK_FPA_DESELECTED:
        text = 'TRK FPA DESELECTED';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.MOVE_THR_LEVERS:
        text = 'MOVE THR LEVERS';
        className = 'BlinkInfinite Amber';
        break;
      case BC3Messages.TD_REACHED:
        text = 'T/D REACHED';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.EXTEND_SPD_BRK:
        text = 'EXTEND SPD BRK';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.RETRACT_SPD_BRK:
        text = 'RETRACT SPD BRK';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.CHECK_APPR_SEL:
        text = 'CHECK APPR SEL';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.SET_HOLD_SPD:
        text = 'SET HOLD SPD';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.EXIT_MISSED:
        text = 'EXIT MISSED';
        className = 'FontMediumSmaller White';
        break;
      case BC3Messages.FCU_ALT_BELOW_AC:
        text = 'FCU ALT BELOW A/C';
        className = 'FontMediumSmaller  White';
        break;
      case BC3Messages.FCU_ALT_ABOVE_AC:
        text = 'FCU ALT ABOVE A/C';
        className = 'FontMediumSmaller White';
        break;
      default:
        return [null, null];
    }

    return [text, className];
  }

  private fillBC3Cell() {
    const [text, className] = this.getBC3MessageText(this.props.BC3Message.get());
    this.classNameSub.set(`FontMedium MiddleAlign ${className}`);
    if (text !== null) {
      this.bc3Cell.target.innerHTML = text;
    } else {
      this.bc3Cell.target.innerHTML = '';
    }
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.props.BC3Message.watch(() => {
      this.fillBC3Cell();
    }, true);
  }

  render(): ViewNode {
    return <text ref={this.bc3Cell} class={this.classNameSub} x="68.087875" y="21.627102" style="white-space: pre" />;
  }
}

class D1D2Cell extends ShowForSecondsComponent<CellProps> {
  private static readonly FiveCharactersPerLineSingleLineModeChangePath = 'm108.1 1.8143h19.994v6.0476h-19.994z';
  private static readonly SixCharactersPerLineTwoLinesModeChangePath = 'm107.1 1.8143h22.994v13.506h-22.994z';
  private static readonly FourCharactersPerLineTwoLinesModeChangePath = 'm110.1 1.8143h15.994v13.506h-15.994z';

  private sub = this.props.bus.inbox<
    PrimFgBusBaseEvents & FcdcBusBaseEvents & FcuEfisCpBusEvents & PFDSimvars
  >();

  private readonly primFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_1'));

  private readonly primFgDiscreteWord2 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_2'));

  private readonly fcdcFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('fcdc_fg_discrete_word_1'));

  private readonly fcuEisDiscreteWord2 = Arinc429LocalVarConsumerSubject.create(null);

  private readonly lsButton = this.fcuEisDiscreteWord2.derive((word) => word.bitValueOr(14, true));

  private readonly hasLoc = BusSignal.from(this.sub.topic('hasLoc'), false);

  private readonly hasGs = BusSignal.from(this.sub.topic('hasGlideslope'), false);

  private readonly appr1Condition = Derived.from(
    ([lsButton, hasLoc, hasGs]) => lsButton && hasGs && hasLoc,
    this.lsButton,
    this.hasLoc,
    this.hasGs,
  );

  private readonly D1D2Message = Derived.from(
    ([primFgDiscreteWord1, primFgDiscreteWord2, fcdcFgDiscreteWord1, appr1Condition]) => {
      return computeD1D2Message(primFgDiscreteWord1, primFgDiscreteWord2, fcdcFgDiscreteWord1, appr1Condition);
    },
    this.primFgDiscreteWord1,
    this.primFgDiscreteWord2,
    this.fcdcFgDiscreteWord1,
    this.appr1Condition,
  );

  private readonly text1Sub = this.D1D2Message.derive((message) => {
    this.isShown = true;

    if (message == D1D2Messages.LAND_2) {
      return 'LAND2';
    } else if (message == D1D2Messages.LAND_3_DUAL || message == D1D2Messages.LAND_3_SINGLE) {
      return 'LAND3';
    } else if (message == D1D2Messages.APPR_1) {
      return 'APPR1';
    } else if (message == D1D2Messages.F_APP || message == D1D2Messages.F_APP_RAW) {
      return 'F-APP';
    } else if (message == D1D2Messages.RAW_ONLY) {
      return 'RAW';
    } else if (message == D1D2Messages.LAND_1) {
      return 'LAND1';
    } else {
      this.isShown = false;

      return '';
    }
  });

  private readonly text2Sub = this.D1D2Message.derive((message) => {
    this.isShown = true;

    if (
      message == D1D2Messages.LAND_1 ||
      message == D1D2Messages.APPR_1 ||
      message == D1D2Messages.LAND_2 ||
      message == D1D2Messages.F_APP
    ) {
      return '';
    } else if (message == D1D2Messages.LAND_3_SINGLE) {
      return 'SINGLE';
    } else if (message == D1D2Messages.LAND_3_DUAL) {
      return 'DUAL';
    } else if (message == D1D2Messages.F_APP_RAW) {
      return '+RAW';
    } else if (message == D1D2Messages.RAW_ONLY) {
      return 'ONLY';
    } else {
      return '';
    }
  });

  private readonly modeChangePath = this.D1D2Message.derive((message) => {
    if (
      message == D1D2Messages.LAND_1 ||
      message == D1D2Messages.APPR_1 ||
      message == D1D2Messages.LAND_2 ||
      message == D1D2Messages.F_APP
    ) {
      return D1D2Cell.FiveCharactersPerLineSingleLineModeChangePath;
    } else if (
      message == D1D2Messages.LAND_3_DUAL ||
      message == D1D2Messages.LAND_3_SINGLE ||
      message == D1D2Messages.F_APP_RAW
    ) {
      return D1D2Cell.SixCharactersPerLineTwoLinesModeChangePath;
    } else if (message == D1D2Messages.RAW_ONLY) {
      return D1D2Cell.FourCharactersPerLineTwoLinesModeChangePath;
    } else {
      return '';
    }
  });

  constructor(props: CellProps) {
    super(props, 9);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    const isFo = getDisplayIndex() === 2;

    this.fcuEisDiscreteWord2.setConsumer(
      this.sub.topic(isFo ? 'fcu_efis_r_discrete_word_2' : 'fcu_efis_l_discrete_word_2'),
    );

    this.D1D2Message.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <text class="FontMedium MiddleAlign White" x="118.45866" y="7.125926">
          {this.text1Sub}
        </text>
        <text class="FontMedium MiddleAlign White" x="118.39752" y="14.289783">
          {this.text2Sub}
        </text>
        <path ref={this.modeChangedPathRef} d={this.modeChangePath} class="NormalStroke White" visibility="hidden" />
      </g>
    );
  }
}

enum MdaMode {
  None = '',
  NoDh = 'NO DH',
  Radio = 'RADIO',
  Baro = 'BARO',
}

class D3Cell extends View<{ bus: ArincEventBus }> {
  private readonly sub = this.props.bus.getArincSubscriber<PFDSimvars & Arinc429Values>();

  private readonly textRef = UI.ref<SVGTextElement>();

  /** bit 29 is NO DH selection */
  private readonly fmEisDiscrete2 = Arinc429LocalVarConsumerSubject.create(this.sub.on('fmEisDiscreteWord2Raw'));

  private readonly mda = Arinc429LocalVarConsumerSubject.create(this.sub.on('fmMdaRaw'));

  private readonly dh = Arinc429LocalVarConsumerSubject.create(this.sub.on('fmDhRaw'));

  private readonly noDhSelected = this.fmEisDiscrete2.derive((r) => r.bitValueOr(29, false));

  private readonly mdaDhMode = Derived.from(
    ([noDh, dh, mda]) => {
      if (noDh) {
        return MdaMode.NoDh;
      }

      if (!dh.isNoComputedData() && !dh.isFailureWarning()) {
        return MdaMode.Radio;
      }

      if (!mda.isNoComputedData() && !mda.isFailureWarning()) {
        return MdaMode.Baro;
      }

      return MdaMode.None;
    },
    this.noDhSelected,
    this.dh,
    this.mda,
  );

  private readonly mdaDhValueText = Derived.from(
    ([mdaMode, dh, mda]) => {
      switch (mdaMode) {
        case MdaMode.Baro:
          return Math.round(mda.value).toString().padStart(6, '\xa0');
        case MdaMode.Radio:
          return Math.round(dh.value).toString().padStart(4, '\xa0');
        default:
          return '';
      }
    },
    this.mdaDhMode,
    this.dh,
    this.mda,
  );
  private readonly DhModexPos = Derived.from(
    ([noDhSelected]) => (noDhSelected ? 118.38384 : 103.47),
    this.noDhSelected,
  );

  afterRender(node: ViewNode): void {
    super.afterRender(node);
  }

  render(): ViewNode {
    return (
      <text
        ref={this.textRef}
        class={{
          FontSmallest: this.noDhSelected.derive(Logic.not()),
          StartAlign: this.noDhSelected.derive(Logic.not()),
          FontMedium: this.noDhSelected,
          MiddleAlign: this.noDhSelected,
          White: true,
        }}
        x={this.DhModexPos}
        y="21.104172"
      >
        <tspan>{this.mdaDhMode}</tspan>
        <tspan
          class={{ EndAlign: true, Cyan: true, HiddenElement: this.mdaDhValueText.derive((v) => v.length <= 0) }}
          x="133.425"
          y="21.104172"
        >
          {this.mdaDhValueText}
        </tspan>
      </text>
    );
  }
}

class E1Cell extends ShowForSecondsComponent<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_1'));

  private ap1Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(11, false));

  private ap2Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(12, false));

  private textSub = Derived.from(
    ([ap1Engaged, ap2Engaged]) => {
      this.isShown = true;
      if (ap1Engaged && ap2Engaged) {
        return 'AP1+2';
      } else if (ap1Engaged) {
        return 'AP1';
      } else if (ap2Engaged) {
        return 'AP2';
      } else {
        this.isShown = false;
        return '';
      }
    },
    this.ap1Engaged,
    this.ap2Engaged,
  );

  constructor(props: CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.textSub.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path
          ref={this.modeChangedPathRef}
          visibility="hidden"
          class="NormalStroke White"
          d="m156.13 1.8143v6.0476h-20.81v-6.0476z"
        />
        <text class="FontMedium MiddleAlign White" x="145.61546" y="6.9559975">
          {this.textSub}
        </text>
      </g>
    );
  }
}

class E2Cell extends ShowForSecondsComponent<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgDiscreteWord1 = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_discrete_word_1'));

  private ap1Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(11, false));

  private ap2Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(12, false));

  private fd1Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(13, false));

  private fd2Engaged = this.primFgDiscreteWord1.derive((word) => word.bitValueOr(14, false));

  private textSub = Derived.from(
    ([ap1Engaged, ap2Engaged, fd1Engaged, fd2Engaged]) => {
      this.isShown = true;
      if (!ap1Engaged && !ap2Engaged && !fd1Engaged && !fd2Engaged) {
        this.isShown = false;
        return '';
      } else {
        return `${fd1Engaged ? '1' : '-'}FD${fd2Engaged ? '2' : '-'}`;
      }
    },
    this.ap1Engaged,
    this.ap2Engaged,
    this.fd1Engaged,
    this.fd2Engaged,
  );

  constructor(props: CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.textSub.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path
          ref={this.modeChangedPathRef}
          d="m156.13 9.0715v6.0476h-20.81v-6.0476z"
          visibility="hidden"
          class="NormalStroke White"
        />
        <text class="FontMedium MiddleAlign White" x="145.95045" style="word-spacing: -1.9844px" y="14.417698">
          {this.textSub}
        </text>
      </g>
    );
  }
}

class E3Cell extends ShowForSecondsComponent<CellProps> {
  private sub = this.props.bus.inbox<PrimFgBusBaseEvents>();

  private primFgAtsDiscreteWord = Arinc429LocalVarConsumerSubject.create(this.sub.topic('prim_fg_ats_discrete_word'));

  private athrEngaged = this.primFgAtsDiscreteWord.derive((word) => word.bitValueOr(11, false));

  private athrActive = this.primFgAtsDiscreteWord.derive((word) => word.bitValueOr(12, false));

  private classSub = Derived.from(
    ([athrEngaged, athrActive]) => {
      this.isShown = true;
      if (athrEngaged && athrActive) {
        this.isShown = false;
        return 'FontMedium MiddleAlign White';
      } else if (athrEngaged) {
        return 'FontMedium MiddleAlign Cyan';
      } else {
        this.isShown = false;
        return 'HiddenElement';
      }
    },
    this.athrEngaged,
    this.athrActive,
  );

  constructor(props: CellProps) {
    super(props, 10);
  }

  afterRender(node: ViewNode): void {
    super.afterRender(node);

    this.classSub.watch(() => {
      this.displayModeChangedPath();
    });
  }

  render(): ViewNode {
    return (
      <g>
        <path
          ref={this.modeChangedPathRef}
          class="NormalStroke White"
          visibility="hidden"
          d="m135.32 16.329h20.81v6.0476h-20.81z"
        />
        <text class={this.classSub} x="145.75578" y="21.434536">
          A/THR
        </text>
      </g>
    );
  }
}
