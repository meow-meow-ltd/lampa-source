import Modal from '../modal'
import Controller from '../../core/controller'
import Lang from '../../core/lang'
import Arrays from '../../utils/arrays'
import ProfileSettings from '../../services/profile_settings'
import AccountListener from '../../core/account/listener'

let menu
let timer
let load_promise = Promise.resolve()
let stored = {
    sort: [],
    hide: []
}
const migration_key = 'menu_profile_migrated'

function init(list){
    menu = list

    load().then(()=>{
        update()
        observe()
    })

    AccountListener.follow('profile_select', ()=>{
        load().then(()=>{
            update()
            observe()
        })
    })
}

function start(){
    let list = $('<div class="menu-edit-list"></div>')

    menu.find('.menu__item').each(function(){
        let item_orig  = $(this)
        let item_clone = $(this).clone()
        let item_sort  = $(`<div class="menu-edit-list__item">
            <div class="menu-edit-list__icon"></div>
            <div class="menu-edit-list__title">${item_clone.find('.menu__text').text()}</div>
            <div class="menu-edit-list__move move-up selector">
                <svg width="22" height="14" viewBox="0 0 22 14" fill="none" xmlns="http://www.w3.org/2000/svg">
                    <path d="M2 12L11 3L20 12" stroke="currentColor" stroke-width="4" stroke-linecap="round"/>
                </svg>
            </div>
            <div class="menu-edit-list__move move-down selector">
                <svg width="22" height="14" viewBox="0 0 22 14" fill="none" xmlns="http://www.w3.org/2000/svg">
                    <path d="M2 2L11 11L20 2" stroke="currentColor" stroke-width="4" stroke-linecap="round"/>
                </svg>
            </div>
            <div class="menu-edit-list__toggle toggle selector">
                <svg width="26" height="26" viewBox="0 0 26 26" fill="none" xmlns="http://www.w3.org/2000/svg">
                    <rect x="1.89111" y="1.78369" width="21.793" height="21.793" rx="3.5" stroke="currentColor" stroke-width="3"/>
                    <path d="M7.44873 12.9658L10.8179 16.3349L18.1269 9.02588" stroke="currentColor" stroke-width="3" class="dot" opacity="0" stroke-linecap="round"/>
                </svg>
            </div>
        </div>`)

        item_sort.find('.menu-edit-list__icon').append(item_clone.find('.menu__ico').html())

        item_sort.find('.move-up').on('hover:enter', ()=>{
            let prev = item_sort.prev()

            if(prev.length){
                item_sort.insertBefore(prev)
                item_orig.insertBefore(item_orig.prev())
            }
        })

        item_sort.find('.move-down').on('hover:enter', ()=>{
            let next = item_sort.next()

            if(next.length){
                item_sort.insertAfter(next)
                item_orig.insertAfter(item_orig.next())
            }
        })

        item_sort.find('.toggle').on('hover:enter', ()=>{
            item_orig.toggleClass('hidden')
            item_sort.find('.dot').attr('opacity', item_orig.hasClass('hidden') ? 0 : 1)
        }).find('.dot').attr('opacity', item_orig.hasClass('hidden') ? 0 : 1)

        list.append(item_sort)
    })

    Modal.open({
        title: Lang.translate('extensions_edit'),
        html: list,
        size: 'small',
        scroll_to_center: true,
        onBack: ()=>{
            Modal.close()

            save()

            Controller.toggle('menu')
        }
    })
}

function order(){
    let items = stored.sort
    
    if(items.length){
        items.forEach((item)=>{
            let el = $('.menu__item:contains("'+item+'")', menu)

            if(el.length) el.appendTo(menu)
        })
    }
}

function hide(){
    let items = stored.hide
    
    $('.menu__item', menu).removeClass('hidden')

    if(items.length){
        items.forEach((item)=>{
            let el = $('.menu__item:contains("'+item+'")', menu)

            if(el.length) el.addClass('hidden')
        })
    }
}

function save(){
    let sort = []
    let hide = []

    $('.menu__item', menu).each(function(){
        let name = $(this).find('.menu__text').text().trim()
        
        sort.push(name)

        if($(this).hasClass('hidden')){
            hide.push(name)
        }
    })

    stored.sort = sort
    stored.hide = hide

    ProfileSettings.set('menu_sort', JSON.stringify(sort))
    ProfileSettings.set('menu_hide', JSON.stringify(hide))
}

function update(){
    order()
    hide()
}

function observe(){
    clearTimeout(timer)
    
    timer = setTimeout(()=>{
        load_promise.then(()=>{
            let memory = stored.sort.slice()
            let anon   = []

            $('.menu__item', menu).each(function(){
                anon.push($(this).find('.menu__text').text().trim())
            })

            anon.forEach((item)=>{
                if(memory.indexOf(item) == -1) memory.push(item)
            })

            stored.sort = memory
            ProfileSettings.set('menu_sort', JSON.stringify(memory))

            update()
        })
    },500)
}

function normalizeList(value){
    if(Array.isArray(value)) return value
    if(typeof value === 'string') return Arrays.decodeJson(value, [])
    return []
}

function readLocalList(key, profile_id){
    if(!profile_id) return null

    if(!shouldMigrate(profile_id)) return null

    let raw = window.localStorage.getItem(key)

    if(raw === null) return null

    markMigrated(profile_id)

    return normalizeList(raw)
}

function load(){
    load_promise = ensureProfile().then(()=>{
        stored.sort = []
        stored.hide = []

        return Promise.all([
            ProfileSettings.get('menu_sort'),
            ProfileSettings.get('menu_hide')
        ])
    }).then(([sort_value, hide_value])=>{
        let sort = normalizeList(sort_value)
        let hide = normalizeList(hide_value)

        if(sort_value === null){
            let local_sort = readLocalList('menu_sort', currentProfileId())

            if(local_sort !== null){
                sort = local_sort
                ProfileSettings.set('menu_sort', JSON.stringify(local_sort))
            }
        }

        if(hide_value === null){
            let local_hide = readLocalList('menu_hide', currentProfileId())

            if(local_hide !== null){
                hide = local_hide
                ProfileSettings.set('menu_hide', JSON.stringify(local_hide))
            }
        }

        stored.sort = sort
        stored.hide = hide
    }).catch(()=>{
        stored.sort = []
        stored.hide = []
    })

    return load_promise
}

function shouldMigrate(profile_id){
    let list = Arrays.decodeJson(window.localStorage.getItem(migration_key), [])

    return list.indexOf(profile_id) === -1
}

function markMigrated(profile_id){
    let list = Arrays.decodeJson(window.localStorage.getItem(migration_key), [])

    if(list.indexOf(profile_id) === -1){
        list.push(profile_id)
        window.localStorage.setItem(migration_key, JSON.stringify(list))
    }
}

function ensureProfile(){
    if(window.Lampa && Lampa.Account && Lampa.Account.Profile && Lampa.Account.Permit){
        if(Lampa.Account.Permit.access && !Lampa.Account.Permit.profileId){
            return new Promise((resolve)=>{
                Lampa.Account.Profile.check(resolve)
            })
        }
    }

    return Promise.resolve()
}

function currentProfileId(){
    if(window.Lampa && Lampa.Account && Lampa.Account.Permit){
        return Lampa.Account.Permit.profileId
    }

    return 0
}

export default {
    init,
    start,
    update,
    observe
}
