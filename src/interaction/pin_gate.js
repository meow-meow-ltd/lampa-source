import Template from './template'
import Controller from '../core/controller'
import Keypad from '../core/keypad'
import Reguest from '../utils/reguest'
import Storage from '../core/storage/storage'
import ID from '../utils/id'
import Lang from '../core/lang'

let html
let digits
let pin = ''
let network = new Reguest()
let keydown_listener
let currentIndex = 0
let verifyCallback = null

function addDigit(num) {
    if (pin.length < 6) {
        pin += num
        update()

        if (pin.length === 6 && verifyCallback) {
            verify(verifyCallback)
        }
    }
}

function removeDigit() {
    if (pin.length > 0) {
        pin = pin.slice(0, -1)
        update()
    }
}

function init() {
    html = Template.get('pin_gate')
    digits = html.find('.pin-gate__digit')
    
    html.find('.pin-gate__key').on('hover:enter', function() {
        let key = $(this).data('key')
        if (key === 'backspace') {
            removeDigit()
        } else {
            addDigit(key)
        }
    }).on('hover:focus', function() {
        html.find('.pin-gate__key').removeClass('focus')
        $(this).addClass('focus')
        // Update currentIndex when a key is focused via mouse/touch
        let keys = html.find('.pin-gate__key')
        currentIndex = keys.index(this)
    })

    $('body').append(html)
}

function applyBlockedUI() {
    html.find('.pin-gate__digits').hide()
    html.find('.pin-gate__keys').hide()
    html.find('.pin-gate__title').text('BLOCKED')
    html.find('.pin-gate__description').text('Please contact the administrator to restore access.')
    html.find('.pin-gate__error').text('')
    
    if (keydown_listener) {
        window.removeEventListener('keydown', keydown_listener)
        keydown_listener = null
    }

    Controller.toggle('empty')
}

function show(callback) {
    if (!html) init()
    
    pin = ''
    verifyCallback = callback
    update()

    if (Storage.get('device_blocked')) {
        console.log('PIN Gate', 'Device is blocked in local storage')
        applyBlockedUI()
        return
    }

    let keys = html.find('.pin-gate__key')
    currentIndex = 0
    let cols = 3

    let focusKey = (index) => {
        if (index >= 0 && index < keys.length) {
            currentIndex = index
            Controller.focus(keys[currentIndex])
        }
    }

    Controller.add('pin_gate', {
        toggle: () => {
            Controller.collectionSet(html)
            Controller.collectionFocus(keys[0], html)
            currentIndex = 0
        },
        up: () => {
            // Special handling for last row (indices 9 and 10)
            // Index 9 (key "0") is in column 1 -> go to index 7 (key "8")
            // Index 10 (backspace) is in column 2 -> go to index 8 (key "9")
            if (currentIndex === 9) {
                focusKey(7)
            } else if (currentIndex === 10) {
                focusKey(8)
            } else {
                let newIndex = currentIndex - cols
                if (newIndex >= 0) {
                    focusKey(newIndex)
                }
            }
        },
        down: () => {
            // Special handling for row 2 (indices 6, 7, 8) going to last row
            // Index 6 (key "7") col 0 -> go to index 9 (key "0", col 1) - closest available
            // Index 7 (key "8") col 1 -> go to index 9 (key "0", col 1)
            // Index 8 (key "9") col 2 -> go to index 10 (backspace, col 2)
            if (currentIndex === 6 || currentIndex === 7) {
                focusKey(9)
            } else if (currentIndex === 8) {
                focusKey(10)
            } else {
                let newIndex = currentIndex + cols
                if (newIndex < keys.length) {
                    focusKey(newIndex)
                }
            }
        },
        left: () => {
            // Special handling for last row
            if (currentIndex === 10) {
                focusKey(9)
            } else if (currentIndex === 9) {
                // No key to the left of "0" in last row, do nothing
            } else if (currentIndex % cols > 0) {
                focusKey(currentIndex - 1)
            }
        },
        right: () => {
            // Special handling for last row
            if (currentIndex === 9) {
                focusKey(10)
            } else if (currentIndex === 10) {
                // No key to the right of backspace, do nothing
            } else if (currentIndex % cols < cols - 1 && currentIndex + 1 < keys.length) {
                focusKey(currentIndex + 1)
            }
        },
        back: () => {
            // Nothing to do
        },
        gone: () => {
            // Nothing to do
        }
    })

    Controller.toggle('pin_gate')

    Keypad.enable()

    keydown_listener = (e) => {
        if (Controller.enabled().name !== 'pin_gate') return

        if (e.keyCode >= 48 && e.keyCode <= 57) {
            addDigit(e.keyCode - 48)
        } else if (e.keyCode >= 96 && e.keyCode <= 105) {
            addDigit(e.keyCode - 96)
        } else if (e.keyCode === 8) { // Backspace
            removeDigit()
        }
    }

    window.addEventListener('keydown', keydown_listener)
}

function update() {
    digits.removeClass('active filled')
    
    for (let i = 0; i < 6; i++) {
        let d = digits.eq(i)
        if (i < pin.length) {
            d.addClass('filled')
        }
        if (i === pin.length) {
            d.addClass('active')
        }
    }
    
    html.find('.pin-gate__error').text('')
}

function verify(callback) {
    html.find('.pin-gate__description').text(Lang.translate('loading') + '...')
    html.find('.pin-gate__keys').css('opacity', 0.5).css('pointer-events', 'none')
    
    network.silent(window.lampa_settings.account_provider === 'local' ? '/api/pin/verify' : 'https://lampa.mx/api/pin/verify', (res) => {
        if (res.ok) {
            document.cookie = "device_token=" + res.device_token + ";path=/;max-age=" + (60*60*24*365)
            hide()
            callback()
        } else {
            pin = ''
            update()
            
            html.find('.pin-gate__keys').css('opacity', 1).css('pointer-events', 'all')

            let error = res.blocked ? 'Blocked. Contact administrator.' : 'Invalid PIN'

            if (res.ip_blocked || res.error === 'too_many_attempts') {
                error = Lang.translate('pin_gate_ip_blocked')
                if (res.retry_after) {
                    error += ' ' + Lang.translate('pin_gate_retry_in').replace('{time}', Math.ceil(res.retry_after / 60))
                }
            } else if (res.attempts_left !== undefined && !res.blocked) {
                error += '. Attempts left: ' + res.attempts_left
            }

            if (res.blocked) {
                console.log('PIN Gate', 'Device blocked, saving state')
                Storage.set('device_blocked', true)
                applyBlockedUI()
            } else {
                html.find('.pin-gate__description').text('Enter 6-digit PIN to access application')
                html.find('.pin-gate__error').text(error)
            }
        }
    }, (err) => {
        pin = ''
        update()
        
        html.find('.pin-gate__keys').css('opacity', 1).css('pointer-events', 'all')

        let error_body = network.errorJSON(err)
        if (error_body && (error_body.ip_blocked || error_body.error === 'too_many_attempts')) {
            let error = Lang.translate('pin_gate_ip_blocked')
            if (error_body.retry_after) {
                error += ' ' + Lang.translate('pin_gate_retry_in').replace('{time}', Math.ceil(error_body.retry_after / 60))
            }
            html.find('.pin-gate__error').text(error)
            html.find('.pin-gate__description').text('Enter 6-digit PIN to access application')
        } else if (error_body && error_body.blocked) {
            Storage.set('device_blocked', true)
            applyBlockedUI()
        } else {
            html.find('.pin-gate__error').text('Server error or unauthorized')
            html.find('.pin-gate__description').text('Enter 6-digit PIN to access application')
        }
    }, {
        pin: pin,
        device_id: ID()
    })
}

function hide() {
    if (html) {
        html.remove()
        html = null
    }
    
    if (keydown_listener) {
        window.removeEventListener('keydown', keydown_listener)
        keydown_listener = null
    }
}

export default {
    show
}
