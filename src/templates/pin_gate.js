let html = `<div class="pin-gate">
    <div class="pin-gate__content">
        <div class="pin-gate__title">PIN GATE</div>
        <div class="pin-gate__description">Enter 6-digit PIN to access application</div>
        
        <div class="pin-gate__digits">
            <div class="pin-gate__digit"></div>
            <div class="pin-gate__digit"></div>
            <div class="pin-gate__digit"></div>
            <div class="pin-gate__digit"></div>
            <div class="pin-gate__digit"></div>
            <div class="pin-gate__digit"></div>
        </div>

        <div class="pin-gate__error"></div>

        <div class="pin-gate__keys">
            <div class="pin-gate__key selector" data-key="1">1</div>
            <div class="pin-gate__key selector" data-key="2">2</div>
            <div class="pin-gate__key selector" data-key="3">3</div>
            <div class="pin-gate__key selector" data-key="4">4</div>
            <div class="pin-gate__key selector" data-key="5">5</div>
            <div class="pin-gate__key selector" data-key="6">6</div>
            <div class="pin-gate__key selector" data-key="7">7</div>
            <div class="pin-gate__key selector" data-key="8">8</div>
            <div class="pin-gate__key selector" data-key="9">9</div>
            <div class="pin-gate__key selector" data-key="0">0</div>
            <div class="pin-gate__key selector" data-key="backspace">
                <svg width="1" height="1" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 4H8l-7 8 7 8h13a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2z"></path><line x1="18" y1="9" x2="12" y2="15"></line><line x1="12" y1="9" x2="18" y2="15"></line></svg>
            </div>
        </div>
    </div>
</div>`

export default html
