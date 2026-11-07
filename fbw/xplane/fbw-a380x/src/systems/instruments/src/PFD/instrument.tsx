// @ts-strict-ignore
import { SimClock, UI, PanelEventPublisher, Board, Signal } from 'xplane-js/ui';
import { ArincEventBus, EfisSide } from '@flybywiresim/fbw-sdk';
import { getDisplayIndex } from '../MsfsAvionicsCommon/CdsDisplayUnit';
import { DmcPublisher } from '../MsfsAvionicsCommon/providers/DmcPublisher';
import { ExtendedClockEventProvider } from '../MsfsAvionicsCommon/providers/ExtendedClockProvider';
import { FmsDataPublisher } from '../MsfsAvionicsCommon/providers/FmsDataPublisher';
import { PFDComponent } from './PFD';
import { AdirsValueProvider } from './shared/AdirsValueProvider';
import { ArincValueProvider } from './shared/ArincValueProvider';
import { PFDSimvarPublisher } from './shared/PFDSimvarPublisher';
import { FcuEfisCpBusPublisher } from '@shared/publishers/EfisCpBusPublisher';
import { FwcPublisher, RopRowOansPublisher, SecPublisher, TawsPublisher } from '@flybywiresim/msfs-avionics-common';
import { FwsPfdSimvarPublisher } from '../MsfsAvionicsCommon/providers/FwsPfdPublisher';
import { FcdcBusPublisher } from '@shared/publishers/FcdcPublisher';
import { FcdcChoiceProvider } from './shared/FcdcChoiceProvider';
import { SfccSimVarPublisher } from '../MsfsAvionicsCommon/providers/SfccPublisher';
import { FGDataPublisher } from '../MsfsAvionicsCommon/providers/FGDataPublisher';
import { FqmsBusPublisher } from '@shared/publishers/FqmsBusPublisher';
import { PrimChoiceProvider } from '../../../shared/src/publishers/PrimChoiceProvider';
import { PrimFeBusPublisher } from '@shared/publishers/PrimFePublisher';
import { PrimFctlBusPublisher } from '@shared/publishers/PrimFctlPublisher';
import { PrimFgBusPublisher } from '@shared/publishers/PrimFgPublisher';
import { FdSelectionProvider } from './shared/FdSelectionProvider';
import { EcuBusPublisher } from '@shared/publishers/EcuPublisher';

import './style.scss';
import { PanelInstrument, defineInstrument } from 'xplane-js/data';

class A380X_PFD extends PanelInstrument {
  private readonly bus = new ArincEventBus();

  private readonly backplane = new Board();

  private readonly clock = new SimClock(this.bus);

  private readonly extendedClockProvider = new ExtendedClockEventProvider(this.bus, Signal.of(true));

  private readonly hEventPublisher = new PanelEventPublisher(this.bus);

  private readonly simVarPublisher = new PFDSimvarPublisher(this.bus);

  private readonly arincProvider = new ArincValueProvider(this.bus);

  private readonly adirsValueProvider = new AdirsValueProvider(this.bus, this.simVarPublisher);

  private readonly dmcPublisher = new DmcPublisher(this.bus);

  private readonly fmsDataPublisher: FmsDataPublisher;

  private readonly fwsPublisher = new FwcPublisher(this.bus);

  private readonly ropRowOansPublisher = new RopRowOansPublisher(this.bus);

  private readonly secPublisher = new SecPublisher(this.bus);

  private readonly tawsPublisher = new TawsPublisher(this.bus);

  private readonly fwsPfdPublisher = new FwsPfdSimvarPublisher(this.bus);

  private readonly fcdcPublisher = new FcdcBusPublisher(this.bus);

  private readonly sfccPublisher = new SfccSimVarPublisher(this.bus);

  private readonly fgDataPublisher = new FGDataPublisher(this.bus);

  private readonly fqmsPublisher = new FqmsBusPublisher(this.bus);

  private readonly fcdcChoiceProvider = new FcdcChoiceProvider(this.bus);

  private readonly primChoiceProvider = new PrimChoiceProvider(this.bus);

  private readonly fdSelectionProvider = new FdSelectionProvider(this.bus);

  private readonly primFePublisher = new PrimFeBusPublisher(this.bus);

  private readonly primFctlPublisher = new PrimFctlBusPublisher(this.bus);

  private readonly primFgPublisher = new PrimFgBusPublisher(this.bus);

  private readonly efisCpBusPublisher = new FcuEfisCpBusPublisher(this.bus);

  private readonly ecuBusPublisher = new EcuBusPublisher(this.bus);

  constructor() {
    super();

    const side: EfisSide = getDisplayIndex() === 1 ? 'L' : 'R';
    const stateSubject = Signal.of<'L' | 'R'>(side);
    this.fmsDataPublisher = new FmsDataPublisher(this.bus, stateSubject);

    this.backplane.attachDevice('Clock', this.clock);
    this.backplane.attachDevice('ExtendedClock', this.extendedClockProvider);
    this.backplane.attachFeeder('HEvent', this.hEventPublisher);
    this.backplane.attachFeeder('PfdSimVars', this.simVarPublisher);
    this.backplane.attachDevice('ArincProvider', this.arincProvider);
    this.backplane.attachDevice('AdirsProvider', this.adirsValueProvider);
    this.backplane.attachFeeder('DmcPublisher', this.dmcPublisher);
    this.backplane.attachFeeder('FmsDataPublisher', this.fmsDataPublisher);
    this.backplane.attachFeeder('FwsPublisher', this.fwsPublisher);
    this.backplane.attachFeeder('RopRowOansPublisher', this.ropRowOansPublisher);
    this.backplane.attachFeeder('SecPublisher', this.secPublisher);
    this.backplane.attachFeeder('TawsPublisher', this.tawsPublisher);
    this.backplane.attachFeeder('FwsPfdPublisher', this.fwsPfdPublisher);
    this.backplane.attachFeeder('FcdcPublisher', this.fcdcPublisher);
    this.backplane.attachDevice('FcdcChoiceProvider', this.fcdcChoiceProvider);
    this.backplane.attachFeeder('SfccPublisher', this.sfccPublisher);
    this.backplane.attachFeeder('FgDataPublisher', this.fgDataPublisher);
    this.backplane.attachFeeder('FqmsPublisher', this.fqmsPublisher);
    this.backplane.attachDevice('FdSelectionProvider', this.fdSelectionProvider);
    this.backplane.attachFeeder('PrimFePublisher', this.primFePublisher);
    this.backplane.attachFeeder('PrimFctlPublisher', this.primFctlPublisher);
    this.backplane.attachFeeder('PrimFgPublisher', this.primFgPublisher);

    this.backplane.attachFeeder('EfisCpPublisher', this.efisCpBusPublisher);

    this.backplane.attachFeeder('EcuPublisher', this.ecuBusPublisher);
  }

  get templateId(): string {
    return 'A380X_PFD';
  }

  public getDeltaTime() {
    return this.frameDelta;
  }

  public onPanelEvent(args: string[]): void {
    this.hEventPublisher.dispatchPanelEvent(args[0]);
  }

  public onAttach(): void {
    super.onAttach();

    this.backplane.setup();
    this.primChoiceProvider.init();

    UI.mount(<PFDComponent bus={this.bus} instrument={this} />, document.getElementById('PFD_CONTENT'));

    // Remove "instrument didn't load" text
    document.getElementById('PFD_CONTENT').querySelector(':scope > h1').remove();
  }

  /**
   * A callback called when the instrument gets a frame update.
   */
  public onFrame(): void {
    super.onFrame();

    this.backplane.tick();
  }
}

defineInstrument('a380x-pfd', A380X_PFD);
