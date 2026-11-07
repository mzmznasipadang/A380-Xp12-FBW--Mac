// Copyright (c) 2021-2026 FlyByWire Simulations
//
// SPDX-License-Identifier: GPL-3.0

import '../index.scss';
import './style.scss';

import { SimClock, UI, Board } from 'xplane-js/ui';
import { ArincValueProvider } from './shared/ArincValueProvider';
import { EwdSimvarPublisher } from './shared/EwdSimvarPublisher';

import { EngineWarningDisplay } from './EWD';
import { AdrBusPublisher, ArincEventBus, IrBusPublisher } from '@flybywiresim/fbw-sdk';
import { FcdcBusPublisher } from '@shared/publishers/FcdcPublisher';
import { FGDataPublisher } from '../MsfsAvionicsCommon/providers/FGDataPublisher';
import { CpiomDataPublisher } from '@providers/CpiomPublisher';
import { PanelInstrument, defineInstrument } from 'xplane-js/data';
import { EcuBusPublisher } from '@shared/publishers/EcuPublisher';

class A380X_EWD extends PanelInstrument {
  private readonly bus = new ArincEventBus();

  private readonly backplane = new Board();

  private readonly simVarPublisher = new EwdSimvarPublisher(this.bus);

  private readonly cpiomPublisher = new CpiomDataPublisher(this.bus);

  private readonly fcdcPublisher = new FcdcBusPublisher(this.bus);

  private readonly arincProvider = new ArincValueProvider(this.bus);

  private readonly adrPublisher = new AdrBusPublisher(this.bus);
  private readonly irPublisher = new IrBusPublisher(this.bus);

  private readonly fgPublisher = new FGDataPublisher(this.bus);

  private readonly ecuPublisher = new EcuBusPublisher(this.bus);

  private readonly clock = new SimClock(this.bus);

  constructor() {
    super();

    this.backplane.attachDevice('Clock', this.clock);
    this.backplane.attachFeeder('SimVars', this.simVarPublisher);
    this.backplane.attachFeeder('CPIOM', this.cpiomPublisher);
    this.backplane.attachFeeder('FCDC', this.fcdcPublisher);
    this.backplane.attachFeeder('ADR', this.adrPublisher);
    this.backplane.attachFeeder('IR', this.irPublisher);
    this.backplane.attachFeeder('FG', this.fgPublisher);
    this.backplane.attachFeeder('ECU', this.ecuPublisher);
  }

  get templateId(): string {
    return 'A380X_EWD';
  }

  public get interactive(): boolean {
    return true;
  }

  public onAttach(): void {
    super.onAttach();

    this.arincProvider.init();
    this.backplane.setup();

    UI.mount(<EngineWarningDisplay bus={this.bus} />, document.getElementById('EWD_CONTENT'));

    // Remove "instrument didn't load" text
    document.getElementById('EWD_CONTENT')?.querySelector(':scope > h1')?.remove();
  }

  public onFrame(): void {
    super.onFrame();

    this.backplane.tick();
  }
}

defineInstrument('a380x-ewd', A380X_EWD);
